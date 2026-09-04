//! The Calendar page: what is planned, what was ridden, and the gap between them.

// Not private: the coaching card offers the same three settlements as the
// calendar does, and one home for them is the only way the toasts, the logging
// and the reload stay identical wherever the rider presses the button.
pub(crate) mod actions;
mod detail;
mod dialogs;
pub(crate) mod marks;
mod month;
mod week;

// Test-only: renders these pages' real widgets to PNG, because GNOME denies
// every screenshot route on this machine. Sited here rather than under `ui/` so
// it can reach `dialogs` without making it public for a tool.
#[cfg(test)]
mod screenshots;

use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::athlete::AthleteProfile;
use crate::data::calendar::{month_bounds, month_label, CalendarEvent};
use crate::data::db::{self};
use crate::data::workout::Workout;
use crate::training::program::{plan_view, CoachVerdict};
use crate::ui::brief_store::BriefStore;

use self::marks::ProgramOverlay;
use crate::ui::ReloadFn;

/// The page-wide reload, resolved at click time rather than at build time.
///
/// The closure a row needs does not exist while that row is being built — see
/// [`crate::ui::ReloadHolder`]. Cloning the inner `Rc` out before calling it
/// matters: reload rebuilds the very widget whose handler is running, and
/// holding the `RefCell` borrow across that is a panic waiting to happen.
fn reload_fn(holder: &Rc<RefCell<Option<ReloadFn>>>) -> Rc<dyn Fn()> {
    let holder = Rc::clone(holder);
    Rc::new(move || {
        let f = holder.borrow().clone();
        if let Some(f) = f {
            f();
        }
    })
}

/// Everything the calendar draws for one visible range.
struct CalendarData {
    /// Scheduled workouts still ahead, anywhere on the calendar — drives the
    /// "ask the coach" banner, so it is not limited to the visible range.
    upcoming: i64,
    events: Vec<CalendarEvent>,
    /// The program's raw state, or `None` when the rider follows no program.
    /// The rules are not run here: they are pure and cheap, and running them on
    /// the main thread keeps the workout library off this future, which has to
    /// be `Send`.
    plan: Option<crate::ui::pages::coaching::data::PlanData>,
}

/// Load one visible range off the GTK main thread (CLAUDE.md §2.3), merging the
/// four sources into a single timeline.
async fn load_calendar_data(
    pool: &SqlitePool,
    today: &str,
    today_date: NaiveDate,
    start: NaiveDate,
    end: NaiveDate,
    ftp: u32,
) -> anyhow::Result<CalendarData> {
    let start_s = start.format("%Y-%m-%d").to_string();
    let end_s = end.format("%Y-%m-%d").to_string();

    let upcoming = db::count_upcoming_scheduled(pool, today).await?;
    let cal_entries = db::load_calendar_entries_between(pool, &start_s, &end_s).await?;
    let past_sessions = db::load_sessions_for_dates(pool, start, end).await?;
    let icu_activities =
        db::load_unlinked_intervals_activities_between(pool, &start_s, &end_s).await?;
    let time_off = db::load_time_off_between(pool, &start_s, &end_s).await?;

    let mut events: Vec<CalendarEvent> = Vec::new();
    events.extend(cal_entries.into_iter().map(CalendarEvent::Scheduled));
    events.extend(past_sessions.into_iter().map(|record| {
        let name = record.workout_name.clone();
        CalendarEvent::Session(record, name)
    }));
    events.extend(icu_activities.into_iter().map(CalendarEvent::IcuActivity));
    events.extend(time_off.into_iter().map(CalendarEvent::TimeOff));

    // Only pay for the program's inputs when there is a program to describe.
    let plan = match db::active_program(pool).await? {
        Some(_) => {
            Some(crate::ui::pages::coaching::data::load_plan_data(pool, today_date, ftp).await?)
        }
        None => None,
    };

    Ok(CalendarData {
        upcoming,
        events,
        plan,
    })
}

pub struct CalendarPage {
    root: gtk::Box,
    /// The morning brief's read on today. It is a reason for the program to
    /// ease, never a way to pick a session — see `training::program::suggest`.
    verdict: Rc<Cell<CoachVerdict>>,
    reload: ReloadFn,
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
        on_start_route: crate::ui::StartRouteHolder,
        athlete: Rc<RefCell<AthleteProfile>>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        on_go_to_coaching: Rc<dyn Fn()>,
    ) -> (Self, Rc<dyn Fn()>) {
        let verdict: Rc<Cell<CoachVerdict>> = Rc::new(Cell::new(CoachVerdict::default()));
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
        let month_label_widget = gtk::Label::builder()
            .label(month_label(today_local.year(), today_local.month()))
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
        nav_row.append(&month_label_widget);
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
            let month_label_widget = month_label_widget.clone();
            let current_month = Rc::clone(&current_month);
            let current_week_start = Rc::clone(&current_week_start);
            let view_mode = Rc::clone(&view_mode);
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let reload_holder = Rc::clone(&reload_holder);
            let workouts = Rc::clone(&workouts);
            let on_start_workout = Rc::clone(&on_start_workout);
            let on_start_route = Rc::clone(&on_start_route);
            let on_toast = Rc::clone(&on_toast);
            let coaching_banner_r = coaching_banner.clone();
            let athlete = Rc::clone(&athlete);
            let verdict = Rc::clone(&verdict);

            Rc::new(move || {
                // Fresh each render: TSS on every row is scaled by FTP, so a
                // profile edit must not leave the calendar showing old numbers.
                let (ftp, weight_kg) = {
                    let a = athlete.borrow();
                    (a.ftp_watts, a.weight_kg)
                };
                let is_week = view_mode.get() == 1;

                // Dates stay typed end to end — they used to be formatted to
                // strings here and re-parsed a few lines later, where a failed
                // parse silently fell back to 1970 and queried the wrong range.
                let (start_date, end_date, label_str) = if is_week {
                    let ws = *current_week_start.borrow();
                    let we = ws + Duration::days(6);
                    (
                        ws,
                        we,
                        format!("{} – {}", ws.format("%-d %b"), we.format("%-d %b %Y")),
                    )
                } else {
                    let (year, month) = current_month.get();
                    let (first, last) =
                        month_bounds(year, month).expect("the visible month is always valid");
                    (first, last, month_label(year, month))
                };

                month_label_widget.set_label(&label_str);

                // Reads run on the tokio runtime and the view is rebuilt on the
                // GTK main thread, so navigation never blocks the GLib loop
                // (CLAUDE.md §2.3).
                // Recomputed per render, not captured at construction: the app
                // is left open across midnight and "today" must move with it.
                let now = Local::now();
                let today_str = now.format("%Y-%m-%d").to_string();
                let today_date = now.date_naive();

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
                let on_start_route_build = Rc::clone(&on_start_route);
                let on_toast = Rc::clone(&on_toast);
                let on_toast_err = Rc::clone(&on_toast);

                let pool_load = pool.clone();
                let today_date_load = today_date;
                let workouts_marks = Rc::clone(&workouts);
                let verdict_now = verdict.get();
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        load_calendar_data(
                            &pool_load,
                            &today_str,
                            today_date_load,
                            start_date,
                            end_date,
                            ftp,
                        )
                        .await
                    },
                    move |result| {
                        // A failed load must not redraw the month as empty — a
                        // blank calendar reads as "nothing planned", which would
                        // have the rider schedule over work already on the books.
                        let CalendarData {
                            upcoming,
                            events,
                            plan,
                        } = match result {
                            Ok(data) => data,
                            Err(e) => {
                                tracing::error!("Could not load the calendar: {e}");
                                on_toast_err(
                                    adw::Toast::builder()
                                        .title("Could not load your calendar")
                                        .timeout(5)
                                        .build(),
                                );
                                return;
                            }
                        };

                        // Show coaching banner when no upcoming scheduled workouts exist.
                        coaching_banner_r.set_revealed(upcoming == 0);

                        // The rules run here, on the main thread: they are pure
                        // and cheap, and this is the one place with both the
                        // workout library and the brief's verdict to hand.
                        let overlay = Rc::new(
                            plan.and_then(|data| {
                                let program = data.program?;
                                let (_, adjustments) = plan_view(
                                    &program,
                                    &data.sessions,
                                    &data.trained,
                                    &data.metrics,
                                    &data.wellness,
                                    &workouts_marks,
                                    today_date_load,
                                    verdict_now,
                                );
                                Some(ProgramOverlay {
                                    program: Some(program),
                                    adjustment: adjustments.into_iter().next(),
                                })
                            })
                            .unwrap_or_default(),
                        );

                        while let Some(child) = dynamic.first_child() {
                            dynamic.remove(&child);
                        }

                        if is_week {
                            let ws = *current_week_start.borrow();
                            dynamic.append(&week::build_week_view(
                                ws,
                                &events,
                                pool_build.clone(),
                                rt_build.clone(),
                                Rc::clone(&reload_holder),
                                Rc::clone(&workouts),
                                Rc::clone(&on_start_workout),
                                Rc::clone(&on_start_route_build),
                                ftp,
                                weight_kg,
                                Rc::clone(&on_toast),
                                Rc::clone(&overlay),
                            ));
                        } else {
                            let (year, month) = current_month.get();
                            dynamic.append(&month::build_month_grid(
                                year,
                                month,
                                &events,
                                pool_build.clone(),
                                rt_build.clone(),
                                Rc::clone(&reload_holder),
                                Rc::clone(&workouts),
                                Rc::clone(&on_start_workout),
                                Rc::clone(&on_start_route_build),
                                ftp,
                                weight_kg,
                                Rc::clone(&on_toast),
                                Rc::clone(&overlay),
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
            let athlete_sb = Rc::clone(&athlete);

            schedule_btn.connect_clicked(move |btn| {
                // Preselect today when viewing the current month, else the 1st.
                let (y, m) = cm.get();
                let today = Local::now().date_naive();
                let preselect = if today.year() == y && today.month() == m {
                    today
                } else {
                    NaiveDate::from_ymd_opt(y, m, 1).expect("valid date")
                };
                dialogs::show_schedule_dialog(
                    btn,
                    &workouts_sb,
                    preselect,
                    pool.clone(),
                    rt_handle.clone(),
                    athlete_sb.borrow().ftp_watts,
                    athlete_sb.borrow().weight_kg,
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
                dialogs::show_time_off_dialog(
                    btn,
                    pool_to.clone(),
                    rt_to.clone(),
                    Rc::clone(&r_to),
                );
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
                                .title(
                                    "No Intervals.icu API key configured — add it in Preferences",
                                )
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
                    // The athlete id is read here rather than on the GTK thread:
                    // a block_on against SQLite stalls the GLib loop if the
                    // database is busy, which it is every 30 s during a ride.
                    let athlete_id = match crate::data::settings::load_intervals(&pool_s).await {
                        Ok(s) if !s.athlete_id.trim().is_empty() => s.athlete_id,
                        Ok(_) => {
                            let _ = tx
                                .send(Err("No Intervals.icu athlete ID configured — \
                                           add it in Preferences"
                                    .to_string()))
                                .await;
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Could not read the Intervals.icu athlete ID: {e}");
                            let _ = tx
                                .send(Err("Could not read your Intervals.icu settings".to_string()))
                                .await;
                            return;
                        }
                    };

                    let today = chrono::Local::now().date_naive();
                    let oldest = today - chrono::Duration::days(60);
                    match crate::ai::intervals::fetch_activities(
                        &athlete_id,
                        &api_key,
                        oldest,
                        today,
                    )
                    .await
                    {
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

        (
            Self {
                root,
                verdict,
                reload: Rc::clone(&reload),
            },
            reload,
        )
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Tell the calendar what the morning brief made of today.
    ///
    /// Only reloads when the value actually changed: the brief store notifies
    /// its observers on every state transition, most of which say nothing new
    /// about the plan and must not cost a round of queries. Same reasoning as
    /// `PlanCard::set_verdict`.
    pub fn set_verdict(&self, verdict: CoachVerdict) {
        if self.verdict.get() == verdict {
            return;
        }
        self.verdict.set(verdict);
        (self.reload)();
    }

    /// Follow the brief store, so an easing appears on the calendar as soon as
    /// the morning brief lands.
    pub fn observe_brief(self: &Rc<Self>, store: &Rc<BriefStore>) {
        let page = Rc::downgrade(self);
        store.observe(move |state: &crate::ui::brief_store::BriefState| {
            if let Some(page) = page.upgrade() {
                page.set_verdict(state.brief.as_ref().map(|b| b.verdict).unwrap_or_default());
            }
        });
    }
}
