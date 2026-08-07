//! The calendar's own dialogs: scheduling a workout, marking time off, and
//! looking at a planned day.

use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate};
use sqlx::SqlitePool;
use std::rc::Rc;

use crate::data::db::{self, CalendarEntry};
use crate::data::workout::Workout;
use crate::ui::widgets::zone_color::{category_zone_rgb, color_stripe};

pub fn show_schedule_dialog(
    parent: &impl IsA<gtk::Widget>,
    workouts: &[Workout],
    preselect: NaiveDate,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    reload: Rc<dyn Fn()>,
) {
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

    sel_row.append(&dropdown);
    content.append(&sel_row);

    // The coach's suggestion is read after the dialog is on screen. Reading
    // it first meant a block_on against SQLite between the click and the
    // dialog appearing (CLAUDE.md §2.3); the preselection just arrives a
    // moment later instead.
    {
        let pool_ai = pool.clone();
        let dropdown_ai = dropdown.clone();
        let content_ai = content.clone();
        let names_ai: Vec<String> = workouts.iter().map(|w| w.name.clone()).collect();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move { db::get_setting(&pool_ai, "ai.suggestion_workout_name").await },
            move |result| {
                let suggestion = match result {
                    Ok(Some(name)) if !name.trim().is_empty() => name,
                    Ok(_) => return,
                    Err(e) => {
                        // Not worth a toast: the rider can still pick a
                        // workout, they just do not get the shortcut.
                        tracing::warn!("Could not read the coach's suggestion: {e}");
                        return;
                    }
                };
                let Some(idx) = names_ai
                    .iter()
                    .position(|n| crate::ai::naming::names_match(n, suggestion.trim()))
                else {
                    return;
                };
                dropdown_ai.set_selected(idx as u32);
                content_ai.append(
                    &gtk::Label::builder()
                        .label(format!("AI Coach suggests: {}", suggestion.trim()))
                        .css_classes(["caption", "accent"])
                        .halign(gtk::Align::Start)
                        .build(),
                );
            },
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

pub fn show_time_off_dialog(
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
            let end = NaiveDate::from_ymd_opt(e.year(), e.month() as u32, e.day_of_month() as u32)
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
        let start = NaiveDate::from_ymd_opt(s.year(), s.month() as u32, s.day_of_month() as u32)
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

pub fn show_workout_detail_dialog(
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

// ── Month summary cards ───────────────────────────────────────────────────

// ── Week view ─────────────────────────────────────────────────────────────
