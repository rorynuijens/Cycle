//! The month grid: a cell per day, each summarising what was planned and done.

use adw::prelude::*;
use chrono::{Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::calendar::{
    days_in_month, first_weekday_column, group_by_date, totals, CalendarEvent,
};
use crate::data::db::{self, CalendarEntry};
use crate::data::workout::Workout;
use crate::ui::widgets::zone_color::category_zone_rgb;

use super::ReloadFn;

#[allow(clippy::too_many_arguments)]
pub fn build_month_grid(
    year: i32,
    month: u32,
    events: &[CalendarEvent],
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    reload_holder: Rc<RefCell<Option<ReloadFn>>>,
    workouts: Rc<Vec<Workout>>,
    on_start_workout: Rc<dyn Fn(Workout)>,
    on_start_route: crate::ui::StartRouteHolder,
    ftp: u32,
    weight_kg: f32,
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
    // Keyed on the full date, so an event from a neighbouring month can
    // never land in this month's cell of the same day number.
    let by_day = group_by_date(events);

    let today = Local::now().date_naive();
    let start_col = first_weekday_column(year, month) as i32;
    let days_in_month = days_in_month(year, month);

    let mut col = start_col;
    let mut row = 1i32;
    // Per-week (grid row) totals for the gutter column.
    let mut week = crate::data::calendar::LoadTotals::default();

    for day_num in 1..=days_in_month {
        let Some(date) = NaiveDate::from_ymd_opt(year, month, day_num) else {
            continue;
        };
        let is_today = date == today;
        let day_events = by_day.get(&date).map(Vec::as_slice).unwrap_or(&[]);

        let items: Vec<MonthCellItem> = day_events
            .iter()
            .map(|e| MonthCellItem::from_event(e, ftp))
            .collect();
        let day_totals = totals(day_events.iter().copied(), ftp);
        week.done_tss += day_totals.done_tss;
        week.planned_tss += day_totals.planned_tss;
        week.scheduled += day_totals.scheduled;
        week.scheduled_done += day_totals.scheduled_done;

        let (cell, item_labels) = make_day_cell(day_num, is_today, &items);
        cell.set_vexpand(true);

        // Helper: the page-wide reload, resolved at click time.
        let make_reload = {
            let rh = Rc::clone(&reload_holder);
            move || -> Rc<dyn Fn()> {
                let rh = Rc::clone(&rh);
                Rc::new(move || {
                    // Clone out before calling: reload rebuilds this grid.
                    let f = rh.borrow().clone();
                    if let Some(f) = f {
                        f();
                    }
                })
            }
        };

        // ── Each planned item opens itself ───────────────────────────────────
        //
        // One gesture per label rather than one for the cell: a cell-wide
        // handler can only open a single entry, so a day with two plans always
        // opened the first.
        for (label, item) in item_labels.iter().zip(items.iter()) {
            let Some(entry) = item.entry.clone() else {
                continue;
            };
            let gesture = gtk::GestureClick::new();
            let pool_i = pool.clone();
            let rt_i = rt_handle.clone();
            let workouts_i = Rc::clone(&workouts);
            let on_start_i = Rc::clone(&on_start_workout);
            let on_start_route_i = Rc::clone(&on_start_route);
            let make_reload_i = make_reload.clone();
            gesture.connect_released(move |g, _, _, _| {
                let Some(widget) = g.widget() else { return };
                super::dialogs::show_workout_detail_dialog(
                    &widget,
                    &entry,
                    pool_i.clone(),
                    rt_i.clone(),
                    Rc::clone(&workouts_i),
                    Rc::clone(&on_start_i),
                    Rc::clone(&on_start_route_i),
                    make_reload_i(),
                );
            });
            label.add_controller(gesture);
            label.set_tooltip_text(Some(&format!("{} — click to open", item.label)));
        }

        // ── Empty future day: click anywhere in it to schedule ───────────────
        let has_plan = items.iter().any(|i| i.entry.is_some());
        if !has_plan && date >= today {
            cell.set_tooltip_text(Some("Schedule a workout or route for this day"));
            let gesture = gtk::GestureClick::new();
            let pool_c = pool.clone();
            let rt_c = rt_handle.clone();
            let workouts_c = Rc::clone(&workouts);
            let make_reload_c = make_reload.clone();
            gesture.connect_released(move |g, _, _, _| {
                let Some(widget) = g.widget() else { return };
                super::dialogs::show_schedule_dialog(
                    &widget,
                    &workouts_c,
                    date,
                    pool_c.clone(),
                    rt_c.clone(),
                    ftp,
                    weight_kg,
                    make_reload_c(),
                );
            });
            cell.add_controller(gesture);
        }

        // ── Drop a dragged plan onto this day ────────────────────────────────
        {
            let drop = gtk::DropTarget::new(i64::static_type(), gtk::gdk::DragAction::MOVE);
            let pool_dt = pool.clone();
            let rt_dt = rt_handle.clone();
            let target_date = date.format("%Y-%m-%d").to_string();
            let make_reload_dt = make_reload.clone();
            drop.connect_drop(move |_, value, _, _| {
                let Ok(entry_id) = value.get::<i64>() else {
                    return false;
                };
                let pool_c = pool_dt.clone();
                let date = target_date.clone();
                let reload = make_reload_dt();
                crate::ui::spawn_to_main(
                    &rt_dt,
                    async move { db::reschedule_entry(&pool_c, entry_id, &date).await },
                    move |res| {
                        match res {
                            Ok(true) => {}
                            Ok(false) => tracing::warn!("entry {entry_id} was not moved"),
                            Err(e) => tracing::error!("reschedule_entry: {e}"),
                        }
                        reload();
                    },
                );
                true
            });
            cell.add_controller(drop);
        }

        grid.attach(&cell, col, row, 1, 1);
        col += 1;
        if col >= 7 || day_num == days_in_month {
            grid.attach(&super::week::week_gutter(&week), 7, row, 1, 1);
            week = crate::data::calendar::LoadTotals::default();
            col = 0;
            row += 1;
        }
    }

    grid
}

/// Builds a day cell, returning it with the label widget for each item it drew.
///
/// The labels come back so the caller can give each one its own click handler:
/// a cell-wide gesture can only ever open one of them, which is why a day with
/// two plans used to open the first whichever you clicked.
fn make_day_cell(
    day_num: u32,
    is_today: bool,
    items: &[MonthCellItem],
) -> (gtk::Frame, Vec<gtk::Label>) {
    let mut item_labels: Vec<gtk::Label> = Vec::new();
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
        let label = gtk::Label::builder()
            .label(&item.label)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(if item.dimmed {
                vec!["dim-label", "caption"]
            } else {
                vec!["caption"]
            })
            .build();

        // An open plan can be dragged straight out of the cell to another day.
        // Completed ones stay put: they record what was actually ridden.
        if let Some(entry) = item.entry.as_ref().filter(|e| !e.completed) {
            let drag = gtk::DragSource::new();
            drag.set_actions(gtk::gdk::DragAction::MOVE);
            let dragged_id = entry.id;
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&dragged_id.to_value()))
            });
            label.add_controller(drag);
        }

        vbox.append(&label);
        item_labels.push(label);
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
            .max_by(|a, b| (a.done_tss + a.planned_tss).total_cmp(&(b.done_tss + b.planned_tss)))
            .and_then(|i| i.color);
        let bar = load_bar(done, planned, dominant);
        bar.set_margin_top(2);
        vbox.append(&bar);
    }

    frame.set_child(Some(&vbox));
    (frame, item_labels)
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
    /// The plan this item came from, when it is one. Carries the id so a single
    /// item can be dragged to another day and opened on its own, instead of the
    /// whole cell standing in for whichever entry happened to be first.
    entry: Option<CalendarEntry>,
}

impl MonthCellItem {
    fn from_event(event: &CalendarEvent, ftp: u32) -> Self {
        let load = event.load(ftp);
        match event {
            CalendarEvent::Scheduled(e) => MonthCellItem {
                label: if e.completed {
                    format!("✓ {}", e.item.name())
                } else {
                    e.item.name().to_string()
                },
                dimmed: e.completed,
                done_tss: load.done_tss,
                planned_tss: load.planned_tss,
                color: category_zone_rgb(&e.category),
                entry: Some((*e).clone()),
            },
            CalendarEvent::Session(_, name) => MonthCellItem {
                label: name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("Ride")
                    .to_string(),
                dimmed: true,
                done_tss: load.done_tss,
                planned_tss: load.planned_tss,
                color: None,
                entry: None,
            },
            CalendarEvent::IcuActivity(a) => MonthCellItem {
                label: if a.name.trim().is_empty() {
                    a.sport_type.clone()
                } else {
                    a.name.clone()
                },
                dimmed: true,
                done_tss: load.done_tss,
                planned_tss: load.planned_tss,
                color: None,
                entry: None,
            },
            CalendarEvent::TimeOff(_) => MonthCellItem {
                label: "Time off".to_string(),
                dimmed: true,
                done_tss: load.done_tss,
                planned_tss: load.planned_tss,
                color: None,
                entry: None,
            },
        }
    }
}

// ── Load bar ──────────────────────────────────────────────────────────────────

/// A day's training load as a slim bar: width ∝ TSS (150 TSS = full width),
/// completed load drawn solid, still-planned load dimmed, coloured by the
/// day's dominant category.
pub fn load_bar(done_tss: f32, planned_tss: f32, rgb: Option<(f64, f64, f64)>) -> gtk::DrawingArea {
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
