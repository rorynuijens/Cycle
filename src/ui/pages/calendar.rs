use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::db::{self, CalendarEntry, IntervalsActivity, SessionRecord, TimeOffEntry};
use crate::data::workout::{Workout, WorkoutCategory};

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

        let clamp = adw::Clamp::builder()
            .maximum_size(960)
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

        let schedule_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Schedule a workout")
            .css_classes(["flat", "circular"])
            .visible(!workouts.is_empty())
            .build();

        let time_off_btn = gtk::Button::builder()
            .icon_name("weather-clear-symbolic")
            .tooltip_text("Mark a day as time off (no cycling)")
            .css_classes(["flat", "circular"])
            .build();

        let import_btn = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text("Import a FIT file recorded on a Garmin, Wahoo, or other device")
            .css_classes(["flat", "circular"])
            .build();

        let icu_sync_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Sync activities from Intervals.icu")
            .css_classes(["flat", "circular"])
            .build();

        // Month / Week toggle — week view is the default
        let month_toggle = gtk::ToggleButton::builder()
            .label("Month")
            .css_classes(["flat"])
            .active(false)
            .tooltip_text("Month view")
            .build();
        let week_toggle = gtk::ToggleButton::builder()
            .label("Week")
            .css_classes(["flat"])
            .tooltip_text("Week view")
            .build();
        week_toggle.set_group(Some(&month_toggle));
        week_toggle.set_active(true);

        nav_row.append(&prev_btn);
        nav_row.append(&month_label);
        nav_row.append(&next_btn);
        nav_row.append(&today_btn);
        nav_row.append(&schedule_btn);
        nav_row.append(&time_off_btn);
        nav_row.append(&import_btn);
        nav_row.append(&icu_sync_btn);
        nav_row.append(&month_toggle);
        nav_row.append(&week_toggle);
        inner.append(&nav_row);

        // ── Dynamic area (summary + grid) — rebuilt on each reload ───────────
        let dynamic = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
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
                            dynamic.append(&Self::build_summary(&events));
                            dynamic.append(&Self::build_month_grid(year, month, &events));
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
                Self::show_schedule_dialog(
                    btn,
                    &workouts_sb,
                    cm.get(),
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
        parent: &gtk::Button,
        workouts: &[Workout],
        (year, month): (i32, u32),
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
        if let Ok(dt) = glib::DateTime::from_local(year, month as i32, 1, 0, 0, 0.0) {
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
        let cat_label = gtk::Label::builder()
            .label(entry.category.label())
            .css_classes(["caption", "pill", category_css_class(&entry.category)])
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

    fn build_summary(events: &[CalendarEvent]) -> gtk::Box {
        let scheduled: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                CalendarEvent::Scheduled(ce) => Some(ce),
                _ => None,
            })
            .collect();

        let total = scheduled.len();
        let done = scheduled.iter().filter(|e| e.completed).count();
        let remaining = total.saturating_sub(done);
        let rate = (done * 100)
            .checked_div(total)
            .map_or_else(|| "—".to_string(), |r| format!("{}%", r));

        let _next_workout = scheduled
            .iter()
            .filter(|e| !e.completed)
            .min_by_key(|e| &e.scheduled_date)
            .and_then(|e| NaiveDate::parse_from_str(&e.scheduled_date, "%Y-%m-%d").ok())
            .map(|d| d.format("%-d %b").to_string())
            .unwrap_or_else(|| "—".to_string());

        let past_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    CalendarEvent::Session(_, _) | CalendarEvent::IcuActivity(_)
                )
            })
            .count();

        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();

        for (title, value, subtitle) in [
            ("Scheduled", total.to_string(), "workouts this period"),
            (
                "Completed",
                done.to_string(),
                &format!("{} remaining", remaining),
            ),
            ("Completion Rate", rate, "of planned workouts"),
            ("Activities", past_count.to_string(), "recorded sessions"),
        ] {
            let card = gtk::Box::builder()
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
                    .label(title)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            vbox.append(
                &gtk::Label::builder()
                    .label(&value)
                    .halign(gtk::Align::Start)
                    .css_classes(["title-3", "numeric"])
                    .build(),
            );
            vbox.append(
                &gtk::Label::builder()
                    .label(subtitle)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            card.append(&vbox);
            row.append(&card);
        }

        row
    }

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
                chip_box.append(
                    &gtk::Label::builder()
                        .label(if is_past { "No activity" } else { "Rest" })
                        .css_classes(["caption", "dim-label"])
                        .halign(gtk::Align::Start)
                        .hexpand(true)
                        .build(),
                );
            } else {
                for event in &non_time_off_events {
                    match event {
                        CalendarEvent::TimeOff(_) => unreachable!(),

                        CalendarEvent::Scheduled(entry) => {
                            let entry_row = gtk::Box::builder()
                                .orientation(gtk::Orientation::Horizontal)
                                .spacing(6)
                                .build();

                            let dot_class = category_css_class(&entry.category);
                            let sport_icon = crate::ui::resources::sport_icon("VirtualRide", true);
                            sport_icon.add_css_class(dot_class);
                            entry_row.append(&sport_icon);

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

                            // Duration badge
                            entry_row.append(
                                &gtk::Label::builder()
                                    .label(format!("{dur_mins} min"))
                                    .css_classes(["caption", "dim-label"])
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

                            let sport_icon = crate::ui::resources::sport_icon("VirtualRide", true);
                            sport_icon.add_css_class("success");
                            entry_row.append(&sport_icon);

                            let display_name = workout_name
                                .as_deref()
                                .filter(|n| !n.is_empty())
                                .unwrap_or("Unstructured Ride");
                            let dur_mins = session.session.duration_secs() / 60;

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

                            let sport_icon =
                                crate::ui::resources::sport_icon(&activity.sport_type, false);
                            sport_icon.add_css_class("accent");
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

    fn build_month_grid(year: i32, month: u32, events: &[CalendarEvent]) -> gtk::Grid {
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .column_homogeneous(true)
            .build();

        for (col, day) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
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
        let mut by_day: HashMap<u32, Vec<MonthCellItem>> = HashMap::new();
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
                by_day
                    .entry(d)
                    .or_default()
                    .push(MonthCellItem::from_event(event));
            }
        }

        let today = Local::now();
        let today_day_opt = if today.year() == year && today.month() == month {
            Some(today.day())
        } else {
            None
        };

        let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid date");
        let start_col = first.weekday().num_days_from_monday() as i32;
        let days_in_month = Self::days_in_month(year, month);

        let mut col = start_col;
        let mut row = 1i32;

        for day_num in 1..=days_in_month {
            let is_today = today_day_opt == Some(day_num);
            let items = by_day.get(&day_num).map(Vec::as_slice).unwrap_or(&[]);
            let cell = Self::make_day_cell(day_num, is_today, items);
            grid.attach(&cell, col, row, 1, 1);
            col += 1;
            if col >= 7 {
                col = 0;
                row += 1;
            }
        }

        grid
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
            if i >= 2 {
                vbox.append(
                    &gtk::Label::builder()
                        .label(format!("+{} more", items.len() - 2))
                        .halign(gtk::Align::Start)
                        .css_classes(["caption", "dim-label"])
                        .build(),
                );
                break;
            }
            let chip = gtk::Label::builder()
                .label(&item.label)
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes([item.css_class, "caption"])
                .build();
            if item.strikethrough {
                let attrs = gtk::pango::AttrList::new();
                attrs.insert(gtk::pango::AttrInt::new_strikethrough(true));
                chip.set_attributes(Some(&attrs));
            }
            vbox.append(&chip);
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
    css_class: &'static str,
    strikethrough: bool,
}

impl MonthCellItem {
    fn from_event(event: &CalendarEvent) -> Self {
        match event {
            CalendarEvent::Scheduled(e) => MonthCellItem {
                label: if e.completed {
                    format!("✓ {}", e.workout_name)
                } else {
                    e.workout_name.clone()
                },
                css_class: if e.completed { "dim-label" } else { "body" },
                strikethrough: false,
            },
            CalendarEvent::Session(_s, name) => MonthCellItem {
                label: name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("Ride")
                    .to_string(),
                css_class: "dim-label",
                strikethrough: false,
            },
            CalendarEvent::IcuActivity(a) => MonthCellItem {
                label: if a.name.trim().is_empty() {
                    a.sport_type.clone()
                } else {
                    a.name.clone()
                },
                css_class: "dim-label",
                strikethrough: false,
            },
            CalendarEvent::TimeOff(_) => MonthCellItem {
                label: "Time off".to_string(),
                css_class: "dim-label",
                strikethrough: false,
            },
        }
    }
}

// ── Category → CSS class ──────────────────────────────────────────────────────

fn category_css_class(cat: &WorkoutCategory) -> &'static str {
    match cat {
        WorkoutCategory::Recovery => "success",
        WorkoutCategory::Endurance => "accent",
        WorkoutCategory::Tempo => "accent",
        WorkoutCategory::SweetSpot => "warning",
        WorkoutCategory::Threshold => "warning",
        WorkoutCategory::Vo2Max => "error",
        WorkoutCategory::Anaerobic => "error",
        WorkoutCategory::Custom => "dim-label",
    }
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
