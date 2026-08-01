use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::db::{self, CalendarEntry, IntervalsActivity, SessionRecord, TimeOffEntry};
use crate::data::workout::Workout;
use crate::ui::widgets::zone_color::{category_zone_rgb, color_stripe};

type ReloadFn = Rc<dyn Fn()>;

/// A unified event shown in the calendar (past or future).
#[derive(Clone)]
enum CalendarEvent {
    Scheduled(CalendarEntry),
    Session(SessionRecord, Option<String>), // session, workout_name
    IcuActivity(IntervalsActivity),
    TimeOff(TimeOffEntry),
}

impl CalendarEvent {
    fn date_str(&self) -> String {
        match self {
            CalendarEvent::Scheduled(e) => e.scheduled_date.clone(),
            CalendarEvent::Session(s, _) => s
                .session
                .started_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string(),
            CalendarEvent::IcuActivity(a) => a.date.format("%Y-%m-%d").to_string(),
            CalendarEvent::TimeOff(t) => t.date.format("%Y-%m-%d").to_string(),
        }
    }

    #[allow(dead_code)]
    fn is_past(&self) -> bool {
        match self {
            CalendarEvent::Session(_, _) | CalendarEvent::IcuActivity(_) => true,
            CalendarEvent::TimeOff(_) => false,
            CalendarEvent::Scheduled(_) => false,
        }
    }
}

pub struct CalendarPage {
    root: gtk::Box,
}

impl CalendarPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` whenever calendar data may
    /// have changed.
    #[allow(clippy::too_many_arguments)] // page constructor wiring; grouping deferred
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        workouts: Vec<Workout>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        ftp: u32,
        weight_kg: f32,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        on_go_to_coaching: Rc<dyn Fn()>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Empty-state coaching banner ──────────────────────────────────────
        let coaching_banner = adw::Banner::builder()
            .title("No upcoming workouts — build a training plan to fill your calendar")
            .button_label("Coaching")
            .revealed(false)
            .build();
        {
            let cb = Rc::clone(&on_go_to_coaching);
            coaching_banner.connect_button_clicked(move |_| cb());
        }
        root.append(&coaching_banner);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        // Wider clamp than the usual ~900: a calendar grid is genuinely a
        // wide layout, and cramped cells waste the page.
        let clamp = adw::Clamp::builder()
            .maximum_size(1400)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Month navigation ─────────────────────────────────────────────────
        let nav_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();

        let prev_btn = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Previous month")
            .css_classes(["flat", "circular"])
            .build();

        let today_local = Local::now();
        let month_label = gtk::Label::builder()
            .label(Self::month_label_str(
                today_local.year(),
                today_local.month(),
            ))
            .css_classes(["title-3"])
            .hexpand(true)
            .halign(gtk::Align::Center)
            .build();

        let next_btn = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Next month")
            .css_classes(["flat", "circular"])
            .build();

        let today_btn = gtk::Button::builder()
            .label("Today")
            .css_classes(["flat"])
            .tooltip_text("Jump to today")
            .build();

        // Scheduling is this page's primary act — a labelled button, not an
        // icon lost among utilities.
        let schedule_btn = gtk::Button::builder()
            .tooltip_text("Schedule a workout")
            .css_classes(["suggested-action"])
            .visible(!workouts.is_empty())
            .build();
        schedule_btn.set_child(Some(
            &adw::ButtonContent::builder()
                .icon_name("list-add-symbolic")
                .label("Schedule")
                .build(),
        ));

        // Rare utilities live behind one menu instead of three toolbar icons.
        let menu_item = |icon: &str, label: &str, tooltip: &str| -> gtk::Button {
            let btn = gtk::Button::builder()
                .css_classes(["flat"])
                .tooltip_text(tooltip)
                .build();
            btn.set_child(Some(
                &adw::ButtonContent::builder()
                    .icon_name(icon)
                    .label(label)
                    .halign(gtk::Align::Start)
                    .build(),
            ));
            btn
        };
        let time_off_btn = menu_item(
            "weather-clear-symbolic",
            "Mark time off",
            "Mark a day as time off (no cycling)",
        );
        let import_btn = menu_item(
            "document-open-symbolic",
            "Import FIT file",
            "Import a FIT file recorded on a Garmin, Wahoo, or other device",
        );
        let icu_sync_btn = menu_item(
            "view-refresh-symbolic",
            "Sync Intervals.icu",
            "Sync activities from Intervals.icu",
        );

        let menu_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        menu_box.append(&time_off_btn);
        menu_box.append(&import_btn);
        menu_box.append(&icu_sync_btn);
        let more_popover = gtk::Popover::builder().child(&menu_box).build();
        for btn in [&time_off_btn, &import_btn, &icu_sync_btn] {
            let popover = more_popover.clone();
            btn.connect_clicked(move |_| popover.popdown());
        }
        let more_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More calendar actions")
            .css_classes(["flat"])
            .popover(&more_popover)
            .build();

        // Month / Week toggle — week view is the default
        let month_toggle = gtk::ToggleButton::builder()
            .label("Month")
            .active(false)
            .tooltip_text("Month view")
            .build();
        let week_toggle = gtk::ToggleButton::builder()
            .label("Week")
            .tooltip_text("Week view")
            .build();
        week_toggle.set_group(Some(&month_toggle));
        week_toggle.set_active(true);
        let view_toggle_box = gtk::Box::builder().css_classes(["linked"]).build();
        view_toggle_box.append(&week_toggle);
        view_toggle_box.append(&month_toggle);

        nav_row.append(&prev_btn);
        nav_row.append(&month_label);
        nav_row.append(&next_btn);
        nav_row.append(&today_btn);
        nav_row.append(&view_toggle_box);
        nav_row.append(&schedule_btn);
        nav_row.append(&more_btn);
        inner.append(&nav_row);

        // ── Dynamic area (grid / week list) — rebuilt on each reload ─────────
        let dynamic = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .vexpand(true)
            .build();
        inner.append(&dynamic);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        // ── State ────────────────────────────────────────────────────────────
        let current_month: Rc<Cell<(i32, u32)>> =
            Rc::new(Cell::new((today_local.year(), today_local.month())));

        let today_date = today_local.date_naive();
        let week_start_init =
            today_date - Duration::days(today_date.weekday().num_days_from_monday() as i64);
        let current_week_start: Rc<RefCell<NaiveDate>> = Rc::new(RefCell::new(week_start_init));

        // 0 = Month, 1 = Week
        let view_mode: Rc<Cell<u8>> = Rc::new(Cell::new(1));

        let reload_holder: Rc<RefCell<Option<ReloadFn>>> = Rc::new(RefCell::new(None));

        let workouts = Rc::new(workouts);
        let on_start_workout = Rc::new(on_start_workout);
        let on_toast = Rc::new(on_toast);

        let reload: Rc<dyn Fn()> = {
            let dynamic = dynamic.clone();
            let month_label = month_label.clone();
            let current_month = Rc::clone(&current_month);
            let current_week_start = Rc::clone(&current_week_start);
            let view_mode = Rc::clone(&view_mode);
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let reload_holder = Rc::clone(&reload_holder);
            let workouts = Rc::clone(&workouts);
            let on_start_workout = Rc::clone(&on_start_workout);
            let on_toast = Rc::clone(&on_toast);
            let coaching_banner_r = coaching_banner.clone();

            Rc::new(move || {
                let is_week = view_mode.get() == 1;

                let (start_str, end_str, label_str) = if is_week {
                    let ws = *current_week_start.borrow();
                    let we = ws + Duration::days(6);
                    (
                        ws.format("%Y-%m-%d").to_string(),
                        we.format("%Y-%m-%d").to_string(),
                        format!("{} – {}", ws.format("%-d %b"), we.format("%-d %b %Y")),
                    )
                } else {
                    let (year, month) = current_month.get();
                    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid date");
                    let last = if month == 12 {
                        NaiveDate::from_ymd_opt(year + 1, 1, 1)
                    } else {
                        NaiveDate::from_ymd_opt(year, month + 1, 1)
                    }
                    .expect("valid date")
                        - Duration::days(1);
                    (
                        first.format("%Y-%m-%d").to_string(),
                        last.format("%Y-%m-%d").to_string(),
                        Self::month_label_str(year, month),
                    )
                };

                month_label.set_label(&label_str);

                // Compute the date range (sync), then run all reads on the tokio
                // runtime and rebuild the view on the GTK main thread so navigation
                // never blocks the GLib loop (CLAUDE.md §2.3).
                let today_str = Local::now().format("%Y-%m-%d").to_string();
                let start_date =
                    NaiveDate::parse_from_str(&start_str, "%Y-%m-%d").unwrap_or_default();
                let end_date = NaiveDate::parse_from_str(&end_str, "%Y-%m-%d").unwrap_or_default();

                // Per-invocation clones for the async result handler (this closure is Fn).
                let dynamic = dynamic.clone();
                let coaching_banner_r = coaching_banner_r.clone();
                let current_month = Rc::clone(&current_month);
                let current_week_start = Rc::clone(&current_week_start);
                let pool_build = pool.clone();
                let rt_build = rt_handle.clone();
                let reload_holder = Rc::clone(&reload_holder);
                let workouts = Rc::clone(&workouts);
                let on_start_workout = Rc::clone(&on_start_workout);
                let on_toast = Rc::clone(&on_toast);

                let pool_load = pool.clone();
                let start_s = start_str.clone();
                let end_s = end_str.clone();
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        let upcoming = db::count_upcoming_scheduled(&pool_load, &today_str)
                            .await
                            .unwrap_or(0);
                        let cal_entries =
                            db::load_calendar_entries_between(&pool_load, &start_s, &end_s)
                                .await
                                .unwrap_or_default();
                        let past_sessions =
                            db::load_sessions_for_dates(&pool_load, start_date, end_date)
                                .await
                                .unwrap_or_default();
                        let icu_activities =
                            db::load_intervals_activities_between(&pool_load, &start_s, &end_s)
                                .await
                                .unwrap_or_default();
                        // Activities already accounted for by a local session — the
                        // same ride returned from Intervals.icu after a round trip
                        // through Garmin or Strava.
                        let linked = db::linked_icu_ids(&pool_load).await.unwrap_or_default();
                        let icu_activities: Vec<_> = icu_activities
                            .into_iter()
                            .filter(|a| !linked.contains(&a.icu_id))
                            .collect();
                        let time_off = db::load_time_off_between(&pool_load, &start_s, &end_s)
                            .await
                            .unwrap_or_default();
                        (
                            upcoming,
                            cal_entries,
                            past_sessions,
                            icu_activities,
                            time_off,
                        )
                    },
                    move |(upcoming, cal_entries, past_sessions, icu_activities, time_off)| {
                        // Show coaching banner when no upcoming scheduled workouts exist.
                        coaching_banner_r.set_revealed(upcoming == 0);

                        // Merge into a unified event list.
                        let mut events: Vec<CalendarEvent> = Vec::new();
                        for e in cal_entries {
                            events.push(CalendarEvent::Scheduled(e));
                        }
                        for s in past_sessions {
                            let name = s.workout_name.clone();
                            events.push(CalendarEvent::Session(s, name));
                        }
                        for a in icu_activities {
                            events.push(CalendarEvent::IcuActivity(a));
                        }
                        for t in time_off {
                            events.push(CalendarEvent::TimeOff(t));
                        }

                        while let Some(child) = dynamic.first_child() {
                            dynamic.remove(&child);
                        }

                        if is_week {
                            let ws = *current_week_start.borrow();
                            dynamic.append(&Self::build_week_view(
                                ws,
                                &events,
                                pool_build.clone(),
                                rt_build.clone(),
                                Rc::clone(&reload_holder),
                                Rc::clone(&workouts),
                                Rc::clone(&on_start_workout),
                                ftp,
                                weight_kg,
                                Rc::clone(&on_toast),
                            ));
                        } else {
                            let (year, month) = current_month.get();
                            dynamic.append(&Self::build_month_grid(
                                year,
                                month,
                                &events,
                                pool_build.clone(),
                                rt_build.clone(),
                                Rc::clone(&reload_holder),
                                Rc::clone(&workouts),
                                Rc::clone(&on_start_workout),
                                ftp,
                            ));
                        }
                    },
                );
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));

        // ── Wire navigation buttons ──────────────────────────────────────────
        {
            let cm = Rc::clone(&current_month);
            let cw = Rc::clone(&current_week_start);
            let vm = Rc::clone(&view_mode);
            let r = Rc::clone(&reload);
            prev_btn.connect_clicked(move |_| {
                if vm.get() == 1 {
                    let new_ws = *cw.borrow() - Duration::weeks(1);
                    *cw.borrow_mut() = new_ws;
                } else {
                    let (y, m) = cm.get();
                    cm.set(if m == 1 { (y - 1, 12) } else { (y, m - 1) });
                }
                r();
            });
        }
        {
            let cm = Rc::clone(&current_month);
            let cw = Rc::clone(&current_week_start);
            let vm = Rc::clone(&view_mode);
            let r = Rc::clone(&reload);
            next_btn.connect_clicked(move |_| {
                if vm.get() == 1 {
                    let new_ws = *cw.borrow() + Duration::weeks(1);
                    *cw.borrow_mut() = new_ws;
                } else {
                    let (y, m) = cm.get();
                    cm.set(if m == 12 { (y + 1, 1) } else { (y, m + 1) });
                }
                r();
            });
        }
        {
            let cm = Rc::clone(&current_month);
            let cw = Rc::clone(&current_week_start);
            let vm = Rc::clone(&view_mode);
            let r = Rc::clone(&reload);
            today_btn.connect_clicked(move |_| {
                let now = Local::now();
                cm.set((now.year(), now.month()));
                let td = now.date_naive();
                *cw.borrow_mut() = td - Duration::days(td.weekday().num_days_from_monday() as i64);
                let _ = vm;
                r();
            });
        }
        {
            let vm = Rc::clone(&view_mode);
            let r = Rc::clone(&reload);
            month_toggle.connect_toggled(move |btn| {
                if btn.is_active() {
                    vm.set(0);
                    r();
                }
            });
        }
        {
            let vm = Rc::clone(&view_mode);
            let r = Rc::clone(&reload);
            week_toggle.connect_toggled(move |btn| {
                if btn.is_active() {
                    vm.set(1);
                    r();
                }
            });
        }

        // ── Schedule workout dialog ──────────────────────────────────────────
        {
            let cm = Rc::clone(&current_month);
            let r = Rc::clone(&reload);
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let workouts_sb = Rc::clone(&workouts);

            schedule_btn.connect_clicked(move |btn| {
                // Preselect today when viewing the current month, else the 1st.
                let (y, m) = cm.get();
                let today = Local::now().date_naive();
                let preselect = if today.year() == y && today.month() == m {
                    today
                } else {
                    NaiveDate::from_ymd_opt(y, m, 1).expect("valid date")
                };
                Self::show_schedule_dialog(
                    btn,
                    &workouts_sb,
                    preselect,
                    pool.clone(),
                    rt_handle.clone(),
                    Rc::clone(&r),
                );
            });
        }

        // ── Time off dialog ──────────────────────────────────────────────────
        {
            let pool_to = pool.clone();
            let rt_to = rt_handle.clone();
            let r_to = Rc::clone(&reload);

            time_off_btn.connect_clicked(move |btn| {
                Self::show_time_off_dialog(btn, pool_to.clone(), rt_to.clone(), Rc::clone(&r_to));
            });
        }

        // ── Import FIT file ───────────────────────────────────────────────────
        crate::ui::widgets::fit_import::connect_fit_import_button(
            &import_btn,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_toast),
            Rc::clone(&reload),
        );

        // ── Intervals.icu sync button ─────────────────────────────────────────
        {
            let pool_icu = pool.clone();
            let rt_icu = rt_handle.clone();
            let reload_icu = Rc::clone(&reload);
            let on_toast_icu = Rc::clone(&on_toast);

            icu_sync_btn.connect_clicked(move |btn| {
                let api_key = match crate::data::keystore::get_secret(
                    crate::data::keystore::KEY_INTERVALS_API,
                ) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        on_toast_icu(
                            adw::Toast::builder()
                                .title("No Intervals.icu API key configured — add it in Preferences")
                                .timeout(5)
                                .build(),
                        );
                        return;
                    }
                };
                let athlete_id = match rt_icu
                    .block_on(crate::data::db::get_setting(&pool_icu, "intervals.athlete_id"))
                    .unwrap_or(None)
                {
                    Some(id) if !id.trim().is_empty() => id,
                    _ => {
                        on_toast_icu(
                            adw::Toast::builder()
                                .title("No Intervals.icu athlete ID configured — add it in Preferences")
                                .timeout(5)
                                .build(),
                        );
                        return;
                    }
                };

                btn.set_sensitive(false);

                let pool_s = pool_icu.clone();
                let reload_s = Rc::clone(&reload_icu);
                let on_toast_s = Rc::clone(&on_toast_icu);
                let btn_s = btn.clone();
                let (tx, rx) = async_channel::bounded::<Result<usize, String>>(1);

                rt_icu.spawn(async move {
                    let today = chrono::Local::now().date_naive();
                    let oldest = today - chrono::Duration::days(60);
                    match crate::ai::intervals::fetch_activities(&athlete_id, &api_key, oldest, today).await {
                        Ok(acts) => {
                            let count = acts.len();
                            for a in acts {
                                let _ = crate::data::db::upsert_intervals_activity(
                                    &pool_s,
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
                        if let Err(e) = crate::data::db::reconcile_icu_links(&pool_s).await {
                            tracing::error!("reconcile_icu_links: {e}");
                        }
                            let _ = tx.send(Ok(count)).await;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string())).await;
                        }
                    }
                });

                glib::MainContext::default().spawn_local(async move {
                    match rx.recv().await {
                        Ok(Ok(count)) => {
                            on_toast_s(
                                adw::Toast::builder()
                                    .title(format!("Synced {count} activities from Intervals.icu"))
                                    .timeout(4)
                                    .build(),
                            );
                            reload_s();
                        }
                        Ok(Err(e)) => {
                            tracing::error!("Intervals.icu calendar sync failed: {e}");
                            on_toast_s(
                                adw::Toast::builder()
                                    .title(format!("Sync failed: {e}"))
                                    .timeout(6)
                                    .build(),
                            );
                        }
                        Err(_) => {}
                    }
                    btn_s.set_sensitive(true);
                });
            });
        }

        // Initial load
        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    // ── Schedule workout dialog ───────────────────────────────────────────────

    fn show_schedule_dialog(
        parent: &impl IsA<gtk::Widget>,
        workouts: &[Workout],
        preselect: NaiveDate,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload: Rc<dyn Fn()>,
    ) {
        let ai_suggestion_name = rt_handle
            .block_on(db::get_setting(&pool, "ai.suggestion_workout_name"))
            .unwrap_or(None)
            .unwrap_or_default();

        let dialog = adw::AlertDialog::builder()
            .heading("Schedule Workout")
            .build();
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("schedule", "_Schedule");
        dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("schedule"));
        dialog.set_close_response("cancel");

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();

        let cal = gtk::Calendar::new();
        if let Ok(dt) = glib::DateTime::from_local(
            preselect.year(),
            preselect.month() as i32,
            preselect.day() as i32,
            0,
            0,
            0.0,
        ) {
            cal.select_day(&dt);
        }
        content.append(&cal);

        let sel_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(6)
            .build();
        sel_row.append(
            &gtk::Label::builder()
                .label("Workout")
                .hexpand(true)
                .halign(gtk::Align::Start)
                .build(),
        );
        let names: Vec<&str> = workouts.iter().map(|w| w.name.as_str()).collect();
        let name_list = gtk::StringList::new(&names);
        let dropdown = gtk::DropDown::builder()
            .model(&name_list)
            .tooltip_text("Select workout to schedule")
            .build();

        let ai_matched = if !ai_suggestion_name.is_empty() {
            workouts
                .iter()
                .position(|w| w.name.eq_ignore_ascii_case(&ai_suggestion_name))
        } else {
            None
        };
        if let Some(idx) = ai_matched {
            dropdown.set_selected(idx as u32);
        }

        sel_row.append(&dropdown);
        content.append(&sel_row);

        if ai_matched.is_some() {
            content.append(
                &gtk::Label::builder()
                    .label(format!("AI Coach suggests: {}", ai_suggestion_name))
                    .css_classes(["caption", "accent"])
                    .halign(gtk::Align::Start)
                    .build(),
            );
        }

        dialog.set_extra_child(Some(&content));

        let workout_ids: Vec<i64> = workouts.iter().map(|w| w.id).collect();
        dialog.connect_response(None, move |_, resp| {
            if resp != "schedule" {
                return;
            }
            let idx = dropdown.selected() as usize;
            let Some(&workout_id) = workout_ids.get(idx) else {
                return;
            };
            let dt = cal.date();
            let date_str = format!(
                "{:04}-{:02}-{:02}",
                dt.year(),
                dt.month(),
                dt.day_of_month()
            );
            let pool = pool.clone();
            let reload = Rc::clone(&reload);
            crate::ui::spawn_to_main(
                &rt_handle,
                async move { db::schedule_workout(&pool, workout_id, &date_str).await },
                move |res| {
                    if let Err(e) = res {
                        tracing::error!("schedule_workout failed: {e}");
                    }
                    reload();
                },
            );
        });

        dialog.present(Some(parent));
    }

    // ── Time off dialog ───────────────────────────────────────────────────────

    fn show_time_off_dialog(
        parent: &gtk::Button,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload: Rc<dyn Fn()>,
    ) {
        let dialog = adw::AlertDialog::builder()
            .heading("Schedule Time Off")
            .body(
                "Mark one or more days as time off. \
                 The AI will suggest non-cycling activities on those days.",
            )
            .build();
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("save", "_Save");
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let now = Local::now();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();

        // Start date
        content.append(
            &gtk::Label::builder()
                .label("Start date")
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let cal_start = gtk::Calendar::new();
        if let Ok(dt) =
            glib::DateTime::from_local(now.year(), now.month() as i32, now.day() as i32, 0, 0, 0.0)
        {
            cal_start.select_day(&dt);
        }
        content.append(&cal_start);

        // Live duration preview label
        let range_label = gtk::Label::builder()
            .label("1 day")
            .halign(gtk::Align::Center)
            .css_classes(["caption", "accent"])
            .build();
        content.append(&range_label);

        // End date
        content.append(
            &gtk::Label::builder()
                .label("End date (same as start for a single day)")
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let cal_end = gtk::Calendar::new();
        if let Ok(dt) =
            glib::DateTime::from_local(now.year(), now.month() as i32, now.day() as i32, 0, 0, 0.0)
        {
            cal_end.select_day(&dt);
        }
        content.append(&cal_end);

        // Notes
        let notes_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let notes_row = adw::EntryRow::builder()
            .title("Notes (optional, e.g. \"Holiday\")")
            .input_hints(gtk::InputHints::NO_EMOJI)
            .build();
        notes_list.append(&notes_row);
        content.append(&notes_list);

        dialog.set_extra_child(Some(&content));

        // Wire calendars → live label
        let update_range_label = {
            let cs = cal_start.clone();
            let ce = cal_end.clone();
            let lbl = range_label.clone();
            move || {
                let s = cs.date();
                let e = ce.date();
                let start =
                    NaiveDate::from_ymd_opt(s.year(), s.month() as u32, s.day_of_month() as u32)
                        .unwrap_or_default();
                let end =
                    NaiveDate::from_ymd_opt(e.year(), e.month() as u32, e.day_of_month() as u32)
                        .unwrap_or_default();
                let days = (end - start).num_days().max(0) + 1;
                lbl.set_label(&format!(
                    "{} {}",
                    days,
                    if days == 1 { "day" } else { "days" }
                ));
            }
        };

        {
            let u = update_range_label.clone();
            cal_start.connect_day_selected(move |_| u());
        }
        {
            let u = update_range_label.clone();
            cal_end.connect_day_selected(move |_| u());
        }

        dialog.connect_response(None, move |_, resp| {
            if resp != "save" {
                return;
            }
            let s = cal_start.date();
            let e = cal_end.date();
            let start =
                NaiveDate::from_ymd_opt(s.year(), s.month() as u32, s.day_of_month() as u32)
                    .unwrap_or_else(|| Local::now().date_naive());
            let end = NaiveDate::from_ymd_opt(e.year(), e.month() as u32, e.day_of_month() as u32)
                .unwrap_or(start);
            // Ensure chronological order regardless of how user picked dates
            let (from, to) = if end >= start {
                (start, end)
            } else {
                (end, start)
            };
            let notes = notes_row.text().trim().to_string();
            let mut days = Vec::new();
            let mut day = from;
            while day <= to {
                days.push(day);
                day += Duration::days(1);
            }
            let pool = pool.clone();
            let reload = Rc::clone(&reload);
            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    for day in days {
                        if let Err(e) = db::save_time_off(&pool, day, &notes).await {
                            tracing::error!("save_time_off failed for {day}: {e}");
                        }
                    }
                },
                move |()| reload(),
            );
        });

        dialog.present(Some(parent));
    }

    // ── Workout detail dialog ─────────────────────────────────────────────────

    fn show_workout_detail_dialog(
        parent: &impl IsA<gtk::Widget>,
        entry: &CalendarEntry,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        workouts: Rc<Vec<Workout>>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        reload: Rc<dyn Fn()>,
    ) {
        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        // Title row
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        title_box.append(
            &gtk::Label::builder()
                .label(&entry.workout_name)
                .css_classes(["title-2"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .wrap(true)
                .build(),
        );
        let cat_stripe = color_stripe(category_zone_rgb(&entry.category));
        cat_stripe.set_content_height(18);
        cat_stripe.set_valign(gtk::Align::Center);
        title_box.append(&cat_stripe);
        let cat_label = gtk::Label::builder()
            .label(entry.category.label())
            .css_classes(["caption", "dim-label"])
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&cat_label);
        content.append(&title_box);

        // Stats row
        let stats_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let dur_mins = entry.duration_secs / 60;
        let dur_row = adw::ActionRow::builder()
            .title("Duration")
            .subtitle(format!("{dur_mins} min"))
            .build();
        let tss_row = adw::ActionRow::builder()
            .title("TSS")
            .subtitle(format!("{:.0}", entry.tss))
            .build();
        stats_list.append(&dur_row);
        stats_list.append(&tss_row);
        content.append(&stats_list);

        if entry.completed {
            content.append(
                &gtk::Label::builder()
                    .label("✓ Completed")
                    .css_classes(["success", "heading"])
                    .halign(gtk::Align::Start)
                    .build(),
            );
        }

        toolbar_view.set_content(Some(&content));

        let dialog = adw::Dialog::builder()
            .title("Workout Details")
            .child(&toolbar_view)
            .content_width(420)
            .build();

        // Action buttons (Load + Remove) — only for incomplete future workouts
        if !entry.completed {
            let btn_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .halign(gtk::Align::End)
                .margin_top(6)
                .build();

            let entry_id = entry.id;
            let workout_name = entry.workout_name.clone();
            let workout_id = entry.workout_id;

            // Load Now
            let load_btn = gtk::Button::builder()
                .label("Load Now")
                .css_classes(["pill", "suggested-action"])
                .tooltip_text("Load this workout and start riding")
                .build();
            let workouts_c = Rc::clone(&workouts);
            let on_start_c = Rc::clone(&on_start_workout);
            let dialog_c = dialog.clone();
            load_btn.connect_clicked(move |_| {
                if let Some(w) = workouts_c.iter().find(|w| w.id == workout_id).cloned() {
                    dialog_c.close();
                    on_start_c(w);
                }
            });

            // Remove
            let remove_btn = gtk::Button::builder()
                .label("Remove")
                .css_classes(["pill", "destructive-action"])
                .tooltip_text("Remove this workout from the calendar")
                .build();
            let pool_r = pool.clone();
            let rt_r = rt_handle.clone();
            let reload_r = Rc::clone(&reload);
            let dialog_r = dialog.clone();
            let remove_btn_for_present = remove_btn.clone();
            remove_btn.connect_clicked(move |_| {
                let pool_c = pool_r.clone();
                let rt_c = rt_r.clone();
                let reload_c = Rc::clone(&reload_r);
                let dialog_c2 = dialog_r.clone();
                crate::ui::widgets::dialog::confirm_destructive(
                    &remove_btn_for_present,
                    "Remove workout?",
                    "This will remove this workout from your calendar.",
                    "_Remove",
                    move || {
                        let pool_c = pool_c.clone();
                        let reload_c = Rc::clone(&reload_c);
                        let dialog_c2 = dialog_c2.clone();
                        crate::ui::spawn_to_main(
                            &rt_c,
                            async move { db::delete_calendar_entry_by_id(&pool_c, entry_id).await },
                            move |res| {
                                if let Err(e) = res {
                                    tracing::error!("delete_calendar_entry_by_id: {e}");
                                }
                                dialog_c2.close();
                                reload_c();
                            },
                        );
                    },
                );
            });

            btn_row.append(&remove_btn);
            btn_row.append(&load_btn);
            content.append(&btn_row);

            // Hint label for workout name
            if let Some(w) = workouts.iter().find(|w| w.id == workout_id) {
                let segments = &w.segments;
                if !segments.is_empty() {
                    content.append(
                        &gtk::Label::builder()
                            .label(format!("{} intervals", segments.len()))
                            .css_classes(["caption", "dim-label"])
                            .halign(gtk::Align::Start)
                            .build(),
                    );
                }
                let _ = workout_name;
            }
        }

        dialog.present(Some(parent));
    }

    // ── Month label helper ────────────────────────────────────────────────────

    fn month_label_str(year: i32, month: u32) -> String {
        NaiveDate::from_ymd_opt(year, month, 1)
            .map(|d| format!("{}", d.format("%B %Y")))
            .unwrap_or_default()
    }

    // ── Month summary cards ───────────────────────────────────────────────────

    // ── Week view ─────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn build_week_view(
        week_start: NaiveDate,
        events: &[CalendarEvent],
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload_holder: Rc<RefCell<Option<ReloadFn>>>,
        workouts: Rc<Vec<Workout>>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        ftp: u32,
        weight_kg: f32,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) -> gtk::Box {
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        // Group events by date string
        let mut by_day: HashMap<String, Vec<&CalendarEvent>> = HashMap::new();
        for event in events {
            by_day.entry(event.date_str()).or_default().push(event);
        }

        let today = Local::now().date_naive();

        for i in 0..7i64 {
            let day = week_start + Duration::days(i);
            let date_str = day.format("%Y-%m-%d").to_string();
            let day_events = by_day.get(&date_str).map(Vec::as_slice).unwrap_or(&[]);
            let is_today = day == today;
            let is_past = day < today;

            let day_frame = gtk::Frame::new(None);
            if is_today {
                day_frame.add_css_class("card");
            }

            let hbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();

            let date_label = gtk::Label::builder()
                .label(day.format("%A\n%-d %b").to_string())
                .css_classes(if is_today {
                    vec!["caption-heading", "accent"]
                } else if is_past {
                    vec!["caption", "dim-label"]
                } else {
                    vec!["caption"]
                })
                .valign(gtk::Align::Center)
                .width_chars(10)
                .xalign(0.0)
                .build();
            hbox.append(&date_label);

            let sep = gtk::Separator::builder()
                .orientation(gtk::Orientation::Vertical)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            hbox.append(&sep);

            // Check for time off — show it as a small indicator but don't skip other events.
            // A day can be marked time off AND still have recorded activities (e.g. a run).
            let time_off_entry = day_events.iter().find_map(|e| {
                if let CalendarEvent::TimeOff(t) = e {
                    Some(t)
                } else {
                    None
                }
            });

            let non_time_off_events: Vec<&CalendarEvent> = day_events
                .iter()
                .copied()
                .filter(|e| !matches!(e, CalendarEvent::TimeOff(_)))
                .collect();

            // Build the content column (time-off badge + any real events)
            let chip_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .hexpand(true)
                .build();

            if let Some(time_off) = time_off_entry {
                let time_off_row = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(6)
                    .build();
                let time_off_label = gtk::Label::builder()
                    .label(if time_off.notes.is_empty() {
                        "Time off".to_string()
                    } else {
                        format!("Time off — {}", time_off.notes)
                    })
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Start)
                    .hexpand(true)
                    .build();
                time_off_row.append(&time_off_label);

                let date_for_remove = day;
                let pool_to = pool.clone();
                let rt_to = rt_handle.clone();
                let rh_to = Rc::clone(&reload_holder);
                let remove_to_btn = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .css_classes(["flat", "circular"])
                    .tooltip_text("Remove time off")
                    .valign(gtk::Align::Center)
                    .build();
                remove_to_btn.connect_clicked(move |_| {
                    let pool_to = pool_to.clone();
                    let rh_to = Rc::clone(&rh_to);
                    crate::ui::spawn_to_main(
                        &rt_to,
                        async move { db::delete_time_off(&pool_to, date_for_remove).await },
                        move |res| {
                            if let Err(e) = res {
                                tracing::error!("delete_time_off: {e}");
                            }
                            if let Some(reload) = rh_to.borrow().as_ref() {
                                reload();
                            }
                        },
                    );
                });
                time_off_row.append(&remove_to_btn);
                chip_box.append(&time_off_row);
            }

            if non_time_off_events.is_empty() && time_off_entry.is_none() {
                if is_past {
                    chip_box.append(
                        &gtk::Label::builder()
                            .label("No activity")
                            .css_classes(["caption", "dim-label"])
                            .halign(gtk::Align::Start)
                            .hexpand(true)
                            .build(),
                    );
                } else {
                    // An empty future day is an invitation to schedule.
                    let rest_btn = gtk::Button::builder()
                        .css_classes(["flat"])
                        .halign(gtk::Align::Start)
                        .hexpand(true)
                        .tooltip_text("Schedule a workout for this day")
                        .build();
                    let rest_content = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(6)
                        .build();
                    rest_content.append(
                        &gtk::Image::builder()
                            .icon_name("list-add-symbolic")
                            .css_classes(["dim-label"])
                            .build(),
                    );
                    rest_content.append(
                        &gtk::Label::builder()
                            .label("Rest — tap to schedule")
                            .css_classes(["caption", "dim-label"])
                            .build(),
                    );
                    rest_btn.set_child(Some(&rest_content));

                    let day_sched = day;
                    let pool_rs = pool.clone();
                    let rt_rs = rt_handle.clone();
                    let rh_rs = Rc::clone(&reload_holder);
                    let workouts_rs = Rc::clone(&workouts);
                    rest_btn.connect_clicked(move |btn| {
                        let rh_c = Rc::clone(&rh_rs);
                        let reload_fn: Rc<dyn Fn()> = Rc::new(move || {
                            if let Some(f) = rh_c.borrow().as_ref() {
                                f();
                            }
                        });
                        Self::show_schedule_dialog(
                            btn,
                            &workouts_rs,
                            day_sched,
                            pool_rs.clone(),
                            rt_rs.clone(),
                            reload_fn,
                        );
                    });
                    chip_box.append(&rest_btn);
                }
            } else {
                for event in &non_time_off_events {
                    match event {
                        CalendarEvent::TimeOff(_) => unreachable!(),

                        CalendarEvent::Scheduled(entry) => {
                            let entry_row = gtk::Box::builder()
                                .orientation(gtk::Orientation::Horizontal)
                                .spacing(6)
                                .build();

                            entry_row.append(&color_stripe(category_zone_rgb(&entry.category)));

                            let display_name = if entry.completed {
                                format!("✓  {}", entry.workout_name)
                            } else {
                                entry.workout_name.clone()
                            };
                            let dur_mins = entry.duration_secs / 60;
                            let tooltip = format!(
                                "{} min · TSS {:.0} · {}",
                                dur_mins,
                                entry.tss,
                                entry.category.label()
                            );

                            let btn = gtk::Button::builder()
                                .css_classes(["flat"])
                                .halign(gtk::Align::Start)
                                .hexpand(true)
                                .tooltip_text(&tooltip)
                                .build();
                            let lbl = gtk::Label::builder()
                                .label(&display_name)
                                .halign(gtk::Align::Start)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .css_classes(if entry.completed {
                                    vec!["caption", "dim-label"]
                                } else {
                                    vec!["caption"]
                                })
                                .build();
                            btn.set_child(Some(&lbl));

                            let entry_clone = (*entry).clone();
                            let pool_d = pool.clone();
                            let rt_d = rt_handle.clone();
                            let workouts_d = Rc::clone(&workouts);
                            let on_start_d = Rc::clone(&on_start_workout);
                            let rh_d = Rc::clone(&reload_holder);
                            btn.connect_clicked(move |b| {
                                let rh_c = Rc::clone(&rh_d);
                                let reload_fn: Rc<dyn Fn()> = Rc::new(move || {
                                    if let Some(f) = rh_c.borrow().as_ref() {
                                        f();
                                    }
                                });
                                Self::show_workout_detail_dialog(
                                    b,
                                    &entry_clone,
                                    pool_d.clone(),
                                    rt_d.clone(),
                                    Rc::clone(&workouts_d),
                                    Rc::clone(&on_start_d),
                                    reload_fn,
                                );
                            });

                            entry_row.append(&btn);

                            // Load out of the tooltip and into view
                            entry_row.append(
                                &gtk::Label::builder()
                                    .label(format!("{} min · TSS {:.0}", dur_mins, entry.tss))
                                    .css_classes(["caption", "dim-label", "numeric"])
                                    .halign(gtk::Align::End)
                                    .build(),
                            );

                            chip_box.append(&entry_row);
                        }

                        CalendarEvent::Session(session, workout_name) => {
                            let entry_row = gtk::Box::builder()
                                .orientation(gtk::Orientation::Horizontal)
                                .spacing(6)
                                .build();

                            entry_row.append(&color_stripe(None));

                            let display_name = workout_name
                                .as_deref()
                                .filter(|n| !n.is_empty())
                                .unwrap_or("Unstructured Ride");
                            let dur_mins = session.session.duration_secs() / 60;
                            let session_tss = session.session.tss(ftp);

                            let btn = gtk::Button::builder()
                                .css_classes(["flat"])
                                .halign(gtk::Align::Start)
                                .hexpand(true)
                                .tooltip_text(format!("{} min · In-app session", dur_mins))
                                .build();
                            btn.set_child(Some(
                                &gtk::Label::builder()
                                    .label(display_name)
                                    .halign(gtk::Align::Start)
                                    .ellipsize(gtk::pango::EllipsizeMode::End)
                                    .css_classes(["caption", "dim-label"])
                                    .build(),
                            ));

                            let session_det = (*session).clone();
                            let title_det = workout_name
                                .as_deref()
                                .filter(|n| !n.is_empty())
                                .unwrap_or("Unstructured Ride")
                                .to_string();
                            let pool_det = pool.clone();
                            let rt_det = rt_handle.clone();
                            let rh_det = Rc::clone(&reload_holder);
                            btn.connect_clicked(move |b| {
                                let local_dt =
                                    session_det.session.started_at.with_timezone(&chrono::Local);
                                let workout = session_det.session.workout_id.and_then(|wid| {
                                    rt_det
                                        .block_on(db::load_workout_by_id(&pool_det, wid))
                                        .ok()
                                        .flatten()
                                });
                                let parent = b.root().and_downcast::<gtk::Window>();
                                super::history::show_session_detail(
                                    &session_det.session,
                                    &title_det,
                                    local_dt,
                                    ftp,
                                    weight_kg,
                                    workout.as_ref(),
                                    parent.as_ref(),
                                    pool_det.clone(),
                                    rt_det.clone(),
                                    Rc::clone(&rh_det),
                                );
                            });

                            entry_row.append(&btn);

                            entry_row.append(
                                &gtk::Label::builder()
                                    .label(match session_tss {
                                        Some(t) => format!("{} min · TSS {:.0}", dur_mins, t),
                                        None => format!("{} min", dur_mins),
                                    })
                                    .css_classes(["caption", "dim-label", "numeric"])
                                    .halign(gtk::Align::End)
                                    .build(),
                            );

                            // Delete button for local sessions
                            let session_id_del = session.session.id;
                            let pool_del = pool.clone();
                            let rt_del = rt_handle.clone();
                            let rh_del = Rc::clone(&reload_holder);
                            let del_btn = gtk::Button::builder()
                                .icon_name("user-trash-symbolic")
                                .tooltip_text("Delete this session")
                                .css_classes(["flat", "circular", "destructive-action"])
                                .valign(gtk::Align::Center)
                                .build();
                            del_btn.connect_clicked(move |btn| {
                                let pool_c = pool_del.clone();
                                let rt_c = rt_del.clone();
                                let rh_c = Rc::clone(&rh_del);
                                crate::ui::widgets::dialog::confirm_destructive(
                                    btn,
                                    "Delete Session?",
                                    "This session and all its data will be permanently deleted.",
                                    "_Delete",
                                    move || {
                                        let pool_c = pool_c.clone();
                                        let rh_c = Rc::clone(&rh_c);
                                        crate::ui::spawn_to_main(
                                            &rt_c,
                                            async move {
                                                db::delete_session(&pool_c, session_id_del).await
                                            },
                                            move |res| {
                                                if let Err(e) = res {
                                                    tracing::error!("delete_session: {e}");
                                                }
                                                if let Some(f) = rh_c.borrow().as_ref() {
                                                    f();
                                                }
                                            },
                                        );
                                    },
                                );
                            });
                            entry_row.append(&del_btn);

                            chip_box.append(&entry_row);
                        }

                        CalendarEvent::IcuActivity(activity) => {
                            let entry_row = gtk::Box::builder()
                                .orientation(gtk::Orientation::Horizontal)
                                .spacing(6)
                                .build();

                            entry_row.append(&color_stripe(None));
                            // Sport icon kept — it distinguishes rides from runs etc.
                            let sport_icon =
                                crate::ui::resources::sport_icon(&activity.sport_type, false);
                            sport_icon.add_css_class("dim-label");
                            entry_row.append(&sport_icon);

                            let display_name = if activity.name.trim().is_empty() {
                                activity.sport_type.clone()
                            } else {
                                activity.name.clone()
                            };
                            let dur_str = activity
                                .duration_secs
                                .map(|s| format!("{} min", s / 60))
                                .unwrap_or_else(|| "—".to_string());

                            let btn = gtk::Button::builder()
                                .css_classes(["flat"])
                                .halign(gtk::Align::Start)
                                .hexpand(true)
                                .tooltip_text(format!("{} · Intervals.icu", dur_str))
                                .build();
                            btn.set_child(Some(
                                &gtk::Label::builder()
                                    .label(&display_name)
                                    .halign(gtk::Align::Start)
                                    .ellipsize(gtk::pango::EllipsizeMode::End)
                                    .css_classes(["caption", "dim-label"])
                                    .build(),
                            ));

                            let activity_det = (*activity).clone();
                            let display_det = display_name.clone();
                            let pool_det = pool.clone();
                            let rt_det = rt_handle.clone();
                            btn.connect_clicked(move |b| {
                                let parent = b.root().and_downcast::<gtk::Window>();
                                super::history::show_intervals_detail(
                                    &activity_det,
                                    &display_det,
                                    ftp,
                                    weight_kg,
                                    &pool_det,
                                    &rt_det,
                                    parent.as_ref(),
                                );
                            });

                            entry_row.append(&btn);

                            entry_row.append(
                                &gtk::Label::builder()
                                    .label(match activity.tss {
                                        Some(t) => format!("{} · TSS {:.0}", dur_str, t),
                                        None => dur_str.clone(),
                                    })
                                    .css_classes(["caption", "dim-label", "numeric"])
                                    .halign(gtk::Align::End)
                                    .build(),
                            );

                            // Delete button for ICU activities
                            let icu_id_del = activity.icu_id.clone();
                            let pool_del = pool.clone();
                            let rt_del = rt_handle.clone();
                            let rh_del = Rc::clone(&reload_holder);
                            let on_toast_del = Rc::clone(&on_toast);
                            let del_btn = gtk::Button::builder()
                                .icon_name("user-trash-symbolic")
                                .tooltip_text("Remove this activity from local history")
                                .css_classes(["flat", "circular", "destructive-action"])
                                .valign(gtk::Align::Center)
                                .build();
                            del_btn.connect_clicked(move |btn| {
                                let pool_c = pool_del.clone();
                                let rt_c = rt_del.clone();
                                let rh_c = Rc::clone(&rh_del);
                                let toast_c = Rc::clone(&on_toast_del);
                                let icu_id_c = icu_id_del.clone();
                                crate::ui::widgets::dialog::confirm_destructive(
                                    btn,
                                    "Remove Activity?",
                                    "This will remove the activity from Cycle's local history. \
                                     It will not be deleted from Intervals.icu.",
                                    "_Remove",
                                    move || {
                                        let pool_c = pool_c.clone();
                                        let rh_c = Rc::clone(&rh_c);
                                        let toast_c = Rc::clone(&toast_c);
                                        let icu_id_c = icu_id_c.clone();
                                        crate::ui::spawn_to_main(
                                            &rt_c,
                                            async move {
                                                db::delete_intervals_activity(&pool_c, &icu_id_c)
                                                    .await
                                            },
                                            move |res| {
                                                if let Err(e) = res {
                                                    tracing::error!(
                                                        "delete_intervals_activity: {e}"
                                                    );
                                                    toast_c(
                                                        adw::Toast::builder()
                                                            .title("Failed to remove activity")
                                                            .timeout(4)
                                                            .build(),
                                                    );
                                                } else {
                                                    toast_c(
                                                        adw::Toast::builder()
                                                            .title(
                                                                "Activity removed from local history",
                                                            )
                                                            .timeout(3)
                                                            .build(),
                                                    );
                                                    if let Some(f) = rh_c.borrow().as_ref() {
                                                        f();
                                                    }
                                                }
                                            },
                                        );
                                    },
                                );
                            });
                            entry_row.append(&del_btn);

                            chip_box.append(&entry_row);
                        }
                    }
                }
            }

            hbox.append(&chip_box);
            day_frame.set_child(Some(&hbox));
            vbox.append(&day_frame);
        }

        vbox
    }

    // ── Month grid ────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn build_month_grid(
        year: i32,
        month: u32,
        events: &[CalendarEvent],
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload_holder: Rc<RefCell<Option<ReloadFn>>>,
        workouts: Rc<Vec<Workout>>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        ftp: u32,
    ) -> gtk::Grid {
        // vexpand: week rows share the viewport's spare height so cells grow
        // with the window instead of huddling at natural size.
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .column_homogeneous(true)
            .vexpand(true)
            .build();

        for (col, day) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Week"]
            .iter()
            .enumerate()
        {
            grid.attach(
                &gtk::Label::builder()
                    .label(*day)
                    .css_classes(["caption", "dim-label"])
                    .build(),
                col as i32,
                0,
                1,
                1,
            );
        }

        // Group events by day-of-month
        let mut by_day: HashMap<u32, Vec<&CalendarEvent>> = HashMap::new();
        for event in events {
            let day_num = match event {
                CalendarEvent::Scheduled(e) => e.scheduled_date[8..].parse::<u32>().ok(),
                CalendarEvent::Session(s, _name) => {
                    let d = s.session.started_at.with_timezone(&chrono::Local).day();
                    Some(d)
                }
                CalendarEvent::IcuActivity(a) => Some(a.date.day()),
                CalendarEvent::TimeOff(t) => Some(t.date.day()),
            };
            if let Some(d) = day_num {
                by_day.entry(d).or_default().push(event);
            }
        }

        let today = Local::now().date_naive();
        let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid date");
        let start_col = first.weekday().num_days_from_monday() as i32;
        let days_in_month = Self::days_in_month(year, month);

        let mut col = start_col;
        let mut row = 1i32;
        // Per-week (grid row) totals for the gutter column.
        let mut week_done_tss = 0.0f32;
        let mut week_planned_tss = 0.0f32;
        let mut week_sched = 0usize;
        let mut week_sched_done = 0usize;

        for day_num in 1..=days_in_month {
            let date = NaiveDate::from_ymd_opt(year, month, day_num).expect("valid date");
            let is_today = date == today;
            let day_events = by_day.get(&day_num).map(Vec::as_slice).unwrap_or(&[]);

            let items: Vec<MonthCellItem> = day_events
                .iter()
                .map(|e| MonthCellItem::from_event(e, ftp))
                .collect();
            for item in &items {
                week_done_tss += item.done_tss;
                week_planned_tss += item.planned_tss;
                if item.is_scheduled {
                    week_sched += 1;
                    if item.planned_tss == 0.0 {
                        week_sched_done += 1;
                    }
                }
            }

            let cell = Self::make_day_cell(day_num, is_today, &items);
            cell.set_vexpand(true);

            // ── Cell interaction: detail for a scheduled day, schedule for an
            // empty/future one. Past days without a plan stay inert.
            let first_scheduled = day_events.iter().find_map(|e| match e {
                CalendarEvent::Scheduled(entry) => Some((*entry).clone()),
                _ => None,
            });
            if first_scheduled.is_some() || date >= today {
                cell.set_tooltip_text(Some(
                    first_scheduled
                        .as_ref()
                        .map(|e| e.workout_name.as_str())
                        .unwrap_or("Schedule a workout for this day"),
                ));
                let gesture = gtk::GestureClick::new();
                let pool_c = pool.clone();
                let rt_c = rt_handle.clone();
                let rh_c = Rc::clone(&reload_holder);
                let workouts_c = Rc::clone(&workouts);
                let on_start_c = Rc::clone(&on_start_workout);
                gesture.connect_released(move |g, _, _, _| {
                    let Some(widget) = g.widget() else { return };
                    let rh = Rc::clone(&rh_c);
                    let reload_fn: Rc<dyn Fn()> = Rc::new(move || {
                        if let Some(f) = rh.borrow().as_ref() {
                            f();
                        }
                    });
                    match &first_scheduled {
                        Some(entry) => Self::show_workout_detail_dialog(
                            &widget,
                            entry,
                            pool_c.clone(),
                            rt_c.clone(),
                            Rc::clone(&workouts_c),
                            Rc::clone(&on_start_c),
                            reload_fn,
                        ),
                        None => Self::show_schedule_dialog(
                            &widget,
                            &workouts_c,
                            date,
                            pool_c.clone(),
                            rt_c.clone(),
                            reload_fn,
                        ),
                    }
                });
                cell.add_controller(gesture);
            }

            grid.attach(&cell, col, row, 1, 1);
            col += 1;
            if col >= 7 || day_num == days_in_month {
                grid.attach(
                    &Self::week_gutter(
                        week_done_tss,
                        week_planned_tss,
                        week_sched_done,
                        week_sched,
                    ),
                    7,
                    row,
                    1,
                    1,
                );
                week_done_tss = 0.0;
                week_planned_tss = 0.0;
                week_sched = 0;
                week_sched_done = 0;
                col = 0;
                row += 1;
            }
        }

        grid
    }

    /// The weekly totals gutter: planned + completed TSS and how much of the
    /// plan got done — the summary cards, condensed to where they're relevant.
    fn week_gutter(done_tss: f32, planned_tss: f32, sched_done: usize, sched: usize) -> gtk::Box {
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .valign(gtk::Align::Center)
            .build();
        let total = done_tss + planned_tss;
        if total > 0.0 {
            vbox.append(
                &gtk::Label::builder()
                    .label(format!("{:.0} TSS", total))
                    .css_classes(["caption-heading", "numeric"])
                    .halign(gtk::Align::Start)
                    .build(),
            );
        }
        if sched > 0 {
            vbox.append(
                &gtk::Label::builder()
                    .label(format!("{}/{} done", sched_done, sched))
                    .css_classes(["caption", "dim-label", "numeric"])
                    .halign(gtk::Align::Start)
                    .build(),
            );
        }
        if total <= 0.0 && sched == 0 {
            vbox.append(
                &gtk::Label::builder()
                    .label("—")
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Start)
                    .build(),
            );
        }
        vbox
    }

    fn make_day_cell(day_num: u32, is_today: bool, items: &[MonthCellItem]) -> gtk::Frame {
        let frame = gtk::Frame::new(None);
        if is_today {
            frame.add_css_class("card");
        }

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(day_num.to_string())
                .halign(gtk::Align::Start)
                .css_classes(if is_today {
                    vec!["accent", "caption-heading"]
                } else {
                    vec!["caption"]
                })
                .build(),
        );

        for (i, item) in items.iter().enumerate() {
            if i >= 3 {
                vbox.append(
                    &gtk::Label::builder()
                        .label(format!("+{} more", items.len() - 3))
                        .halign(gtk::Align::Start)
                        .css_classes(["caption", "dim-label"])
                        .build(),
                );
                break;
            }
            vbox.append(
                &gtk::Label::builder()
                    .label(&item.label)
                    .halign(gtk::Align::Start)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(if item.dimmed {
                        vec!["dim-label", "caption"]
                    } else {
                        vec!["caption"]
                    })
                    .build(),
            );
        }

        // Spacer keeps the day number pinned to the top and the load bar to
        // the bottom edge when the cell stretches with the window.
        vbox.append(&gtk::Box::builder().vexpand(true).build());

        // The signature: the day's load as a zone-coloured bar.
        let done: f32 = items.iter().map(|i| i.done_tss).sum();
        let planned: f32 = items.iter().map(|i| i.planned_tss).sum();
        if done + planned > 0.0 {
            // Dominant category = the coloured item carrying the most TSS.
            let dominant = items
                .iter()
                .filter(|i| i.color.is_some())
                .max_by(|a, b| {
                    (a.done_tss + a.planned_tss).total_cmp(&(b.done_tss + b.planned_tss))
                })
                .and_then(|i| i.color);
            let bar = load_bar(done, planned, dominant);
            bar.set_margin_top(2);
            vbox.append(&bar);
        }

        frame.set_child(Some(&vbox));
        frame
    }

    fn days_in_month(year: i32, month: u32) -> u32 {
        let next = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        };
        next.unwrap()
            .signed_duration_since(NaiveDate::from_ymd_opt(year, month, 1).unwrap())
            .num_days() as u32
    }
}

// ── Month cell item ───────────────────────────────────────────────────────────

struct MonthCellItem {
    label: String,
    dimmed: bool,
    /// TSS already banked (completed scheduled workouts, sessions, activities).
    done_tss: f32,
    /// TSS still ahead (scheduled and not yet completed).
    planned_tss: f32,
    /// Category zone colour — only scheduled workouts carry one.
    color: Option<(f64, f64, f64)>,
    is_scheduled: bool,
}

impl MonthCellItem {
    fn from_event(event: &CalendarEvent, ftp: u32) -> Self {
        match event {
            CalendarEvent::Scheduled(e) => MonthCellItem {
                label: if e.completed {
                    format!("✓ {}", e.workout_name)
                } else {
                    e.workout_name.clone()
                },
                dimmed: e.completed,
                done_tss: if e.completed { e.tss } else { 0.0 },
                planned_tss: if e.completed { 0.0 } else { e.tss },
                color: category_zone_rgb(&e.category),
                is_scheduled: true,
            },
            CalendarEvent::Session(s, name) => MonthCellItem {
                label: name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("Ride")
                    .to_string(),
                dimmed: true,
                done_tss: s.session.tss(ftp).unwrap_or(0.0),
                planned_tss: 0.0,
                color: None,
                is_scheduled: false,
            },
            CalendarEvent::IcuActivity(a) => MonthCellItem {
                label: if a.name.trim().is_empty() {
                    a.sport_type.clone()
                } else {
                    a.name.clone()
                },
                dimmed: true,
                done_tss: a.tss.unwrap_or(0.0),
                planned_tss: 0.0,
                color: None,
                is_scheduled: false,
            },
            CalendarEvent::TimeOff(_) => MonthCellItem {
                label: "Time off".to_string(),
                dimmed: true,
                done_tss: 0.0,
                planned_tss: 0.0,
                color: None,
                is_scheduled: false,
            },
        }
    }
}

// ── Load bar ──────────────────────────────────────────────────────────────────

/// A day's training load as a slim bar: width ∝ TSS (150 TSS = full width),
/// completed load drawn solid, still-planned load dimmed, coloured by the
/// day's dominant category.
fn load_bar(done_tss: f32, planned_tss: f32, rgb: Option<(f64, f64, f64)>) -> gtk::DrawingArea {
    const FULL_SCALE_TSS: f64 = 150.0;
    let area = gtk::DrawingArea::builder()
        .content_height(6)
        .hexpand(true)
        .build();
    area.set_draw_func(move |widget, cr, width, height| {
        let total = (done_tss + planned_tss) as f64;
        if total <= 0.0 {
            return;
        }
        let w = width as f64;
        let h = height as f64;
        let (r, g, b) = rgb.unwrap_or_else(|| {
            let fg = widget.color();
            (fg.red() as f64, fg.green() as f64, fg.blue() as f64)
        });
        let bar_w = (total / FULL_SCALE_TSS).min(1.0) * w;
        let done_w = bar_w * (done_tss as f64 / total);
        // Completed load: solid
        cr.set_source_rgba(r, g, b, 1.0);
        cr.rectangle(0.0, 0.0, done_w, h);
        cr.fill().ok();
        // Planned load: dimmed continuation
        cr.set_source_rgba(r, g, b, 0.35);
        cr.rectangle(done_w, 0.0, bar_w - done_w, h);
        cr.fill().ok();
    });
    area
}

// ── Build time-off context string for AI prompts ──────────────────────────────

/// Returns a short plain-text string listing upcoming time-off dates for AI prompts.
#[allow(dead_code)]
pub fn format_time_off_for_prompt(entries: &[db::TimeOffEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let dates: Vec<String> = entries
        .iter()
        .map(|e| {
            if e.notes.is_empty() {
                e.date.format("%Y-%m-%d").to_string()
            } else {
                format!("{} ({})", e.date.format("%Y-%m-%d"), e.notes)
            }
        })
        .collect();
    format!("TIME OFF (no cycling): {}", dates.join(", "))
}
