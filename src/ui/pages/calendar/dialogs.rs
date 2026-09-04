//! The calendar's own dialogs: scheduling a workout, marking time off, and
//! looking at a planned day.

use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::data::ai_cache;
use crate::data::db::{self, CalendarEntry};
use crate::data::route::Route;
use crate::data::workout::Workout;
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::widgets::zone_color::{category_zone_rgb, color_stripe};

use super::marks::EntryMark;

/// Dress the completion row for a state, whether it is being built or redrawn.
///
/// One function for both, because the dialog stays open across a change: two
/// copies of this — one at build time, one in the click handler — is how a row
/// ends up saying "Marked done" beside a button offering "Mark done".
fn dress_done_row(row: &adw::ActionRow, icon: &gtk::Image, btn: &gtk::Button, done: bool) {
    row.set_title(if done { "Marked done" } else { "Not done yet" });
    row.set_subtitle(if done {
        "This session counts towards your program."
    } else {
        "Rode this away from the app? Mark it done here."
    });
    icon.set_icon_name(Some(if done {
        "object-select-symbolic"
    } else {
        "media-playlist-consecutive-symbolic"
    }));
    icon.set_css_classes(if done { &["success"] } else { &["dim-label"] });
    icon.update_property(&[gtk::accessible::Property::Label(if done {
        "Marked done"
    } else {
        "Not done yet"
    })]);
    btn.set_label(if done { "Mark not done" } else { "Mark done" });
    btn.set_tooltip_text(Some(if done {
        "Put this session back to not done"
    } else {
        "Mark this session done without riding it here"
    }));
}

/// What the easing rows are currently saying, so a redraw can restate it.
///
/// The dialog is opened with a snapshot and stays open across a write, so this
/// is the one place that tracks what has changed since.
struct EasingState {
    /// The workout now on the day — it changes under the dialog when eased.
    entry_name: String,
    mark: EntryMark,
    /// Whether the suggestion the dialog opened with is still on offer.
    suggestion_standing: bool,
}

/// Dress both easing rows for a state, whether building or redrawing.
///
/// One function for both, for the reason given on [`dress_done_row`]: two
/// copies drift, and here the drift is an Undo button naming a workout the
/// entry has already been put back to.
fn dress_easing_rows(
    list: &gtk::ListBox,
    undo_row: &adw::ActionRow,
    undo_btn: &gtk::Button,
    apply_row: &adw::ActionRow,
    apply_btn: &gtk::Button,
    state: &EasingState,
) {
    let eased = state.mark.adjusted_from.is_some();
    undo_row.set_visible(eased);
    if let Some(original) = &state.mark.adjusted_from {
        // The row names the rung, the subtitle names the origin: after two eases
        // those are different workouts, and the rider needs both — where the
        // plan started, and where one press lands. The destination is in the row
        // rather than on the button because a button carrying a workout name
        // truncates to "Undo — back to R…" at every width this dialog has.
        undo_row.set_title(&state.mark.undo_title());
        undo_row.set_subtitle(&format!("Originally {original}"));
        undo_btn.set_label("Undo");
        undo_btn.set_tooltip_text(Some(&format!(
            "Put this session back one step, to {}",
            state.mark.undo_target().unwrap_or(original.as_str())
        )));
    }

    let offering = state.suggestion_standing;
    apply_row.set_visible(offering);
    if let (true, Some(suggestion)) = (offering, &state.mark.suggestion) {
        apply_row.set_title(&format!("{} → {}", state.entry_name, suggestion.to_name));
        apply_btn.set_tooltip_text(Some(&format!(
            "Ease this session to {}",
            suggestion.to_name
        )));
    }

    list.set_visible(eased || offering);
}

/// What the rider picked in the scheduling dialog.
#[derive(Clone, Copy)]
enum Picked {
    Workout(i64),
    /// Carries the route's id; the load estimate needs the GPX and is worked out
    /// when scheduling, not while building the list.
    Route(i64),
}

/// Schedule a workout or a GPX route on a chosen day.
///
/// The list shows what each choice actually costs — a workout's shape, duration
/// and TSS; a route's distance and climbing — because a name alone is not enough
/// to choose from a library of a hundred.
#[allow(clippy::too_many_arguments)] // dialog wiring
pub fn show_schedule_dialog(
    parent: &impl IsA<gtk::Widget>,
    workouts: &[Workout],
    preselect: NaiveDate,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    ftp: u32,
    weight_kg: f32,
    reload: Rc<dyn Fn()>,
) {
    let dialog = adw::AlertDialog::builder().heading("Schedule").build();
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

    // What the active program wants on the chosen day, filled in below and left
    // hidden when there is no program to compare against.
    let plan_hint = gtk::Label::builder()
        .css_classes(["caption", "dim-label"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(false)
        .build();
    content.append(&plan_hint);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search workouts and routes")
        .build();
    content.append(&search);

    // The picked item, read by the response handler. Rc<Cell> rather than a
    // widget lookup so the two lists can share one notion of the selection.
    let picked: Rc<Cell<Option<Picked>>> = Rc::new(Cell::new(None));

    let workout_list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    let route_list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::Single)
        .build();

    // ── Workout rows ─────────────────────────────────────────────────────────
    let mut workout_ids: Vec<i64> = Vec::with_capacity(workouts.len());
    for w in workouts {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&w.name))
            .subtitle(glib::markup_escape_text(
                &super::super::library::workout_list::row_subtitle(w),
            ))
            .build();
        // The same thumbnail the library list uses, so a workout looks the same
        // wherever it is chosen.
        let thumb = WorkoutGraph::new(w, ftp);
        thumb.widget().set_content_width(84);
        thumb.widget().set_content_height(42);
        thumb.widget().set_valign(gtk::Align::Center);
        row.add_prefix(thumb.widget());
        let stripe = color_stripe(category_zone_rgb(&w.category));
        stripe.set_content_height(28);
        stripe.set_valign(gtk::Align::Center);
        row.add_prefix(&stripe);
        workout_list.append(&row);
        workout_ids.push(w.id);
    }

    let workouts_group = adw::PreferencesGroup::builder().title("Workouts").build();
    workouts_group.add(&workout_list);
    let routes_group = adw::PreferencesGroup::builder().title("Routes").build();
    routes_group.add(&route_list);
    // Hidden until routes are known to exist, so a rider with none never sees an
    // empty heading.
    routes_group.set_visible(false);

    let lists = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    lists.append(&workouts_group);
    lists.append(&routes_group);

    // A ScrolledWindow reports its minimum as its natural height, so without a
    // floor the whole dialog collapses to a sliver.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(300)
        .max_content_height(420)
        .propagate_natural_height(true)
        .child(&lists)
        .build();
    content.append(&scroller);

    // ── Selection: one choice across two lists ───────────────────────────────
    {
        let picked_c = Rc::clone(&picked);
        let ids = workout_ids.clone();
        let other = route_list.clone();
        workout_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            if let Some(&id) = ids.get(row.index() as usize) {
                picked_c.set(Some(Picked::Workout(id)));
                other.unselect_all();
            }
        });
    }

    // ── Routes, loaded after the dialog is on screen ─────────────────────────
    //
    // Read here rather than before presenting so the click does not wait on
    // SQLite (CLAUDE.md §2.3).
    let route_ids: Rc<RefCell<Vec<i64>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let pool_r = pool.clone();
        let route_list_c = route_list.clone();
        let routes_group_c = routes_group.clone();
        let route_ids_c = Rc::clone(&route_ids);
        let picked_c = Rc::clone(&picked);
        let workout_list_c = workout_list.clone();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move { db::load_routes(&pool_r).await },
            move |result| {
                let routes = match result {
                    Ok(r) if !r.is_empty() => r,
                    Ok(_) => return,
                    Err(e) => {
                        tracing::warn!("Could not read your routes: {e}");
                        return;
                    }
                };
                for r in &routes {
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&r.name))
                        .subtitle(format!(
                            "{:.1} km · {:.0} m climb",
                            r.distance_m / 1000.0,
                            r.elevation_gain_m
                        ))
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("map-symbolic"));
                    route_list_c.append(&row);
                    route_ids_c.borrow_mut().push(r.id);
                }
                routes_group_c.set_visible(true);

                let ids = Rc::clone(&route_ids_c);
                let picked_sel = Rc::clone(&picked_c);
                let other = workout_list_c.clone();
                route_list_c.connect_row_selected(move |_, row| {
                    let Some(row) = row else { return };
                    let id = ids.borrow().get(row.index() as usize).copied();
                    if let Some(id) = id {
                        picked_sel.set(Some(Picked::Route(id)));
                        other.unselect_all();
                    }
                });
            },
        );
    }

    // ── Search filters both lists ────────────────────────────────────────────
    {
        let workout_list_f = workout_list.clone();
        let route_list_f = route_list.clone();
        search.connect_search_changed(move |entry| {
            let needle = entry.text().to_lowercase();
            for list in [&workout_list_f, &route_list_f] {
                let mut child = list.first_child();
                while let Some(row) = child {
                    child = row.next_sibling();
                    let matches = needle.is_empty()
                        || row
                            .downcast_ref::<adw::ActionRow>()
                            .map(|r| r.title().to_lowercase().contains(&needle))
                            .unwrap_or(true);
                    row.set_visible(matches);
                }
            }
        });
    }

    // ── What the plan asks for on the chosen day ─────────────────────────────
    {
        let pool_p = pool.clone();
        let rt_p = rt_handle.clone();
        let plan_hint_c = plan_hint.clone();
        let cal_p = cal.clone();
        let update = move || {
            let dt = cal_p.date();
            let date = format!(
                "{:04}-{:02}-{:02}",
                dt.year(),
                dt.month(),
                dt.day_of_month()
            );
            let pool_c = pool_p.clone();
            let hint = plan_hint_c.clone();
            crate::ui::spawn_to_main(
                &rt_p,
                async move {
                    let program = db::active_program(&pool_c).await.ok().flatten()?;
                    let sessions = db::load_program_sessions(&pool_c, program.id).await.ok()?;
                    sessions
                        .into_iter()
                        .find(|s| s.date.format("%Y-%m-%d").to_string() == date)
                        .map(|s| (s.workout_name, s.tss))
                },
                move |found| match found {
                    Some((name, tss)) => {
                        hint.set_label(&format!(
                            "Your plan asks for {name} ({tss:.0} TSS) that day"
                        ));
                        hint.set_visible(true);
                    }
                    None => hint.set_visible(false),
                },
            );
        };
        update();
        let update_on_change = update.clone();
        cal.connect_day_selected(move |_| update_on_change());
    }

    dialog.set_extra_child(Some(&content));

    // The coach's suggestion is read after the dialog is on screen. Reading it
    // first meant a block_on against SQLite between the click and the dialog
    // appearing (CLAUDE.md §2.3); the preselection just arrives a moment later.
    {
        let pool_ai = pool.clone();
        let content_ai = content.clone();
        let list_ai = workout_list.clone();
        let names_ai: Vec<String> = workouts.iter().map(|w| w.name.clone()).collect();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move {
                let today = chrono::Local::now()
                    .date_naive()
                    .format("%Y-%m-%d")
                    .to_string();
                ai_cache::brief_workout_name(&pool_ai, &today).await
            },
            move |result| {
                let suggestion = match result {
                    Ok(Some(name)) if !name.trim().is_empty() => name,
                    Ok(_) => return,
                    Err(e) => {
                        // Not worth a toast: the rider can still pick a workout,
                        // they just do not get the shortcut.
                        tracing::warn!("Could not read the morning brief's workout: {e}");
                        return;
                    }
                };
                let Some(idx) = names_ai
                    .iter()
                    .position(|n| crate::ai::naming::names_match(n, suggestion.trim()))
                else {
                    return;
                };
                if let Some(row) = list_ai.row_at_index(idx as i32) {
                    list_ai.select_row(Some(&row));
                }
                content_ai.append(
                    &gtk::Label::builder()
                        .label(format!("Your morning brief: {}", suggestion.trim()))
                        .css_classes(["caption", "accent"])
                        .halign(gtk::Align::Start)
                        .build(),
                );
            },
        );
    }

    dialog.connect_response(None, move |_, resp| {
        if resp != "schedule" {
            return;
        }
        let Some(choice) = picked.get() else {
            // Nothing selected: closing without scheduling is the honest outcome.
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
            async move {
                match choice {
                    Picked::Workout(id) => db::schedule_workout(&pool, id, &date_str, None)
                        .await
                        .map(|_| ()),
                    Picked::Route(id) => {
                        // The estimate needs the route's points, so the GPX is
                        // parsed here — once, off the main thread — and the
                        // result stored on the row rather than recomputed on
                        // every calendar redraw.
                        let (secs, tss) = estimate_route_load(&pool, id, ftp, weight_kg).await?;
                        db::schedule_route(&pool, id, &date_str, tss, secs)
                            .await
                            .map(|_| ())
                    }
                }
            },
            move |res| {
                if let Err(e) = res {
                    tracing::error!("scheduling failed: {e}");
                }
                reload();
            },
        );
    });

    dialog.present(Some(parent));
}

/// Read a saved route's GPX from disk.
async fn load_saved_route(pool: &SqlitePool, route_id: i64) -> anyhow::Result<Route> {
    let routes = db::load_routes(pool).await?;
    let saved = routes
        .into_iter()
        .find(|r| r.id == route_id)
        .ok_or_else(|| anyhow::anyhow!("route {route_id} is no longer in the library"))?;
    Route::from_gpx_path(&db::routes_dir()?.join(&saved.file_name))
}

/// Parse a saved route's GPX and estimate what riding it will cost.
///
/// Returns `(duration_secs, tss)`. Runs off the GTK thread: reading and parsing
/// a GPX is file I/O plus a full XML pass.
async fn estimate_route_load(
    pool: &SqlitePool,
    route_id: i64,
    ftp: u32,
    weight_kg: f32,
) -> anyhow::Result<(u32, f32)> {
    let route = load_saved_route(pool, route_id).await?;
    Ok(route.estimated_load(ftp, weight_kg))
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

#[allow(clippy::too_many_arguments)] // dialog wiring
pub fn show_workout_detail_dialog(
    parent: &impl IsA<gtk::Widget>,
    entry: &CalendarEntry,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    workouts: Rc<Vec<Workout>>,
    on_start_workout: Rc<dyn Fn(Workout)>,
    on_start_route: crate::ui::StartRouteHolder,
    reload: Rc<dyn Fn()>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    mark: EntryMark,
    rides: Vec<crate::training::matching::DayRide>,
    ftp: u32,
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
            .label(entry.item.name())
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

    if let Some(line) = mark.program_text() {
        content.append(
            &gtk::Label::builder()
                .label(line)
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .build(),
        );
    }

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

    // ── What you actually rode that day ──────────────────────────────────────
    //
    // Stated, not asked. The program reads a day with real training on it as
    // trained by itself (`training::matching::trained_days`), so there is
    // nothing here for the rider to confirm — this only says what the app is
    // already going on, so a day that stops counting as missed does not do so
    // silently.
    //
    // It was briefly a list of buttons asking which ride "was" the planned
    // session. On a day holding an 89-minute road ride against a planned
    // 30-minute recovery spin there is no honest answer to that, and the
    // question only existed because `completed` had two states and needed a
    // third.
    if !rides.is_empty() {
        let rides_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        for ride in &rides {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&ride.name))
                .subtitle(ride.summary())
                .subtitle_lines(0)
                .build();
            let icon = gtk::Image::from_icon_name("emblem-shared-symbolic");
            icon.set_css_classes(&["accent"]);
            icon.update_property(&[gtk::accessible::Property::Label(
                "A ride you did on this day",
            )]);
            row.add_prefix(&icon);
            rides_list.append(&row);
        }
        content.append(
            &gtk::Label::builder()
                .label("You rode this day")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        content.append(&rides_list);
        content.append(
            &gtk::Label::builder()
                .label("This day counts as trained, whether or not you did the planned session.")
                .halign(gtk::Align::Start)
                .wrap(true)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }

    // ── Done, or not done ────────────────────────────────────────────────────
    //
    // A row rather than a label, because it has to work in both directions: the
    // way back matters as much as the way in, and the button row further down is
    // hidden entirely once an entry is completed, so an un-mark control placed
    // there would vanish exactly when it is wanted.
    //
    // This says whether the *planned session* was done, which is a narrower
    // question than whether the day was trained on. Nothing above depends on it.
    let done_list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    let done_row = adw::ActionRow::builder().subtitle_lines(0).build();
    let done_icon = gtk::Image::new();
    done_row.add_prefix(&done_icon);
    let done_btn = gtk::Button::builder()
        .css_classes(["pill"])
        .valign(gtk::Align::Center)
        .build();
    done_row.add_suffix(&done_btn);
    dress_done_row(&done_row, &done_icon, &done_btn, entry.completed);
    done_list.append(&done_row);
    content.append(&done_list);

    {
        // The dialog does not close on a change, so it has to redraw itself:
        // `reload` refreshes the calendar underneath, and would leave this row
        // stating the opposite of what was just written.
        //
        // Every widget is held weakly. The handler lives on a button the dialog
        // owns, and the dialog owns the row it sits in, so a strong capture here
        // is a reference cycle with nothing to collect it (CLAUDE.md §2.4). The
        // button comes back as the handler's own argument, weakly, for the same
        // reason.
        let row_w = done_row.downgrade();
        let icon_w = done_icon.downgrade();
        let btn_w = done_btn.downgrade();
        // The row is a toggle and the rider may press it twice, so the state
        // has to live somewhere that survives the first press. The entry this
        // dialog was opened with is a snapshot and stops being true.
        let state = Rc::new(Cell::new(entry.completed));

        let pool_d = pool.clone();
        let rt_d = rt_handle.clone();
        let reload_d = Rc::clone(&reload);
        let toast_d = Rc::clone(&on_toast);
        let entry_id = entry.id;
        done_btn.connect_clicked(move |_| {
            let want = !state.get();

            let (row_w, icon_w, btn_w) = (row_w.clone(), icon_w.clone(), btn_w.clone());
            let state = Rc::clone(&state);
            // Only on a write that landed: an entry deleted underneath us must
            // leave the row alone and let the toast explain.
            let on_settled: Rc<dyn Fn(bool)> = Rc::new(move |now| {
                state.set(now);
                if let (Some(row), Some(icon), Some(btn)) =
                    (row_w.upgrade(), icon_w.upgrade(), btn_w.upgrade())
                {
                    dress_done_row(&row, &icon, &btn, now);
                }
            });

            super::actions::set_session_done(
                pool_d.clone(),
                &rt_d,
                entry_id,
                want,
                Rc::clone(&toast_d),
                Rc::clone(&reload_d),
                Some(on_settled),
            );
        });
    }

    // ── What the program wants changed ───────────────────────────────────────
    //
    // Deliberately not a suggested-action: the row below already has one (Load
    // Now / Ride Route), and the HIG allows only one per view. The card carries
    // its own emphasis through the icon and the accent group instead.
    let easing_list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .visible(false)
        .build();

    // A session can be both: eased once already, and the rules still want it
    // easier. Both rows exist, because the rider needs the way back as much as
    // the way forward.
    //
    // They are built unconditionally and hidden, never appended conditionally.
    // The dialog stays open across a change, so a session that gains an
    // "already eased" state by being eased from right here needs a row waiting
    // to be dressed — building it only when the opening snapshot called for it
    // is how the rider ends up pressing Apply and seeing nothing happen.
    let undo_row = adw::ActionRow::builder()
        .subtitle_lines(0)
        .visible(false)
        .build();
    let undo_icon = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .css_classes(["success"])
        .build();
    undo_icon.update_property(&[gtk::accessible::Property::Label("Already eased")]);
    undo_row.add_prefix(&undo_icon);
    let undo_btn = gtk::Button::builder()
        .css_classes(["pill"])
        .valign(gtk::Align::Center)
        .build();
    undo_row.add_suffix(&undo_btn);
    easing_list.append(&undo_row);

    let apply_row = adw::ActionRow::builder()
        .subtitle(
            mark.suggestion
                .as_ref()
                .map(|s| s.reason.as_str())
                .unwrap_or_default(),
        )
        .subtitle_lines(0)
        .visible(false)
        .build();
    let apply_icon = gtk::Image::builder()
        .icon_name("view-refresh-symbolic")
        .css_classes(["warning"])
        .build();
    apply_icon.update_property(&[gtk::accessible::Property::Label(
        "Your program suggests easing this session",
    )]);
    apply_row.add_prefix(&apply_icon);
    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .css_classes(["pill"])
        .valign(gtk::Align::Center)
        .build();
    apply_row.add_suffix(&apply_btn);
    easing_list.append(&apply_row);

    let easing_state = Rc::new(RefCell::new(EasingState {
        entry_name: entry.item.name().to_string(),
        mark: mark.clone(),
        suggestion_standing: mark.suggestion.is_some(),
    }));

    // Built before the dialog and holding only weak widget references: a strong
    // capture would put the rows inside a closure the rows themselves reach
    // (CLAUDE.md §2.4).
    let redraw_easing: Rc<dyn Fn()> = {
        let list_w = easing_list.downgrade();
        let undo_row_w = undo_row.downgrade();
        let undo_btn_w = undo_btn.downgrade();
        let apply_row_w = apply_row.downgrade();
        let apply_btn_w = apply_btn.downgrade();
        let state = Rc::clone(&easing_state);
        Rc::new(move || {
            if let (Some(list), Some(ur), Some(ub), Some(ar), Some(ab)) = (
                list_w.upgrade(),
                undo_row_w.upgrade(),
                undo_btn_w.upgrade(),
                apply_row_w.upgrade(),
                apply_btn_w.upgrade(),
            ) {
                dress_easing_rows(&list, &ur, &ub, &ar, &ab, &state.borrow());
            }
        })
    };
    redraw_easing();

    // Re-read the entry after a write and dress the rows from what came back.
    //
    // Only the entry is re-read. Recomputing the suggestion would need the whole
    // `plan_view` — program, sessions, metrics, wellness, verdict — none of
    // which this dialog holds; `reload` rebuilds the surfaces that do.
    let refresh_easing: Rc<dyn Fn()> = {
        let pool_r = pool.clone();
        let rt_r = rt_handle.clone();
        let state = Rc::clone(&easing_state);
        let redraw = Rc::clone(&redraw_easing);
        let entry_id = entry.id;
        Rc::new(move || {
            let pool_c = pool_r.clone();
            let state = Rc::clone(&state);
            let redraw = Rc::clone(&redraw);
            crate::ui::spawn_to_main(
                &rt_r,
                async move { db::load_calendar_entry_by_id(&pool_c, entry_id).await },
                move |res| {
                    match res {
                        Ok(Some(fresh)) => {
                            let mut st = state.borrow_mut();
                            st.entry_name = fresh.item.name().to_string();
                            st.mark.adjusted_from = fresh.adjusted_from;
                            st.mark.previous_step_name = fresh.previous_step_name;
                            // Dropped before the redraw: the closure borrows the
                            // same cell, and a live borrow here would panic.
                            drop(st);
                            redraw();
                        }
                        // Deleted from under the dialog. The toast and `reload`
                        // already say so; redrawing from a guess would not.
                        Ok(None) => {
                            tracing::warn!("entry {entry_id} is gone; rows left as they are")
                        }
                        Err(e) => tracing::error!("re-reading entry {entry_id}: {e}"),
                    }
                },
            );
        })
    };

    {
        let pool_u = pool.clone();
        let rt_u = rt_handle.clone();
        let reload_u = Rc::clone(&reload);
        let toast_u = Rc::clone(&on_toast);
        let refresh_u = Rc::clone(&refresh_easing);
        let entry_id = entry.id;
        undo_btn.connect_clicked(move |_| {
            super::actions::undo_easing(
                pool_u.clone(),
                &rt_u,
                entry_id,
                Rc::clone(&toast_u),
                Rc::clone(&reload_u),
                Some(Rc::clone(&refresh_u)),
            );
        });
    }

    if let Some(suggestion) = mark.suggestion.clone() {
        let pool_a = pool.clone();
        let rt_a = rt_handle.clone();
        let reload_a = Rc::clone(&reload);
        let toast_a = Rc::clone(&on_toast);
        let refresh_a = Rc::clone(&refresh_easing);
        let state_a = Rc::clone(&easing_state);
        let entry_id = entry.id;
        apply_btn.connect_clicked(move |_| {
            let state_a = Rc::clone(&state_a);
            let refresh_a = Rc::clone(&refresh_a);
            let on_settled: Rc<dyn Fn()> = Rc::new(move || {
                // The offer is spent. It cannot be recomputed here, and leaving
                // it on screen invites a second press that would ease the same
                // day twice, in one step the program never proposed.
                //
                // Only the flag is set here; the redraw waits for the re-read,
                // so the rows change once rather than blanking and coming back.
                state_a.borrow_mut().suggestion_standing = false;
                refresh_a();
            });
            super::actions::apply_easing(
                pool_a.clone(),
                &rt_a,
                entry_id,
                suggestion.to_workout_id,
                suggestion.to_name.clone(),
                Rc::clone(&toast_a),
                Rc::clone(&reload_a),
                Some(on_settled),
            );
        });
    }
    content.append(&easing_list);

    // ── The shape of the session ─────────────────────────────────────────────
    //
    // This is the surface where the rider decides whether to ride the thing, and
    // it was the only one of the app's seven `WorkoutGraph` surfaces without a
    // profile. Not gated on `completed`: a finished day is exactly where someone
    // looks to see what they did.
    //
    // A scheduled route has no profile to draw, and neither has a workout with
    // no segments — `WorkoutGraph::draw` divides by `duration_secs`, so a
    // zero-length workout renders as a bare dashed FTP line and nothing else.
    let planned_workout = entry
        .item
        .workout_id()
        .and_then(|id| workouts.iter().find(|w| w.id == id));
    if let Some(workout) = planned_workout.filter(|w| !w.segments.is_empty()) {
        content.append(
            &gtk::Label::builder()
                .label("Power Profile")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        let graph = WorkoutGraph::new(workout, ftp);
        let graph_widget = graph.widget().clone();
        // `WorkoutGraph::new` asks for 600 px, which would shove this 420 px
        // dialog out to roughly 650. The library's copy escapes that only by
        // sitting in a ScrolledWindow; here the request is simply overridden.
        graph_widget.set_content_width(1);
        graph_widget.set_hexpand(true);
        graph_widget.set_accessible_role(gtk::AccessibleRole::Img);
        graph_widget.update_property(&[gtk::accessible::Property::Label("Workout power profile")]);
        content.append(&graph_widget);

        // Pure description, so it sits with the graph rather than in the
        // action block below — where being gated on `!completed` took it away
        // from a day the moment it was marked done.
        content.append(
            &gtk::Label::builder()
                .label(format!("{} intervals", workout.segments.len()))
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .build(),
        );
    }

    // Scrollable, because the content is no longer bounded: a day can be eased
    // *and* have a suggestion waiting *and* list the rides it was trained on
    // *and* now carry a graph, which on a short window would otherwise run off
    // the bottom with no way to reach it.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&content)
        .build();
    toolbar_view.set_content(Some(&scroller));

    let dialog = adw::Dialog::builder()
        .title(match entry.item {
            db::ScheduledItem::Workout { .. } => "Workout Details",
            db::ScheduledItem::Route { .. } => "Route Details",
        })
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
        // `None` for a scheduled route: it is ridden in the route player, which
        // this dialog does not reach, so it gets no Load button.
        let workout_id = entry.item.workout_id();

        // Reschedule — move this entry to another day without losing it.
        let move_btn = gtk::Button::builder()
            .label("Reschedule")
            .css_classes(["pill"])
            .tooltip_text("Move this session to another day")
            .build();
        {
            let pool_m = pool.clone();
            let rt_m = rt_handle.clone();
            let reload_m = Rc::clone(&reload);
            let current_date = entry.scheduled_date.clone();
            let move_btn_for_present = move_btn.clone();
            // Weak: this button is inside the dialog (CLAUDE.md §2.4).
            move_btn.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    show_reschedule_dialog(
                        &move_btn_for_present,
                        entry_id,
                        &current_date,
                        pool_m.clone(),
                        rt_m.clone(),
                        Rc::clone(&reload_m),
                        dialog.clone(),
                    );
                }
            ));
        }

        // Load Now — workouts only.
        let load_btn = gtk::Button::builder()
            .label("Load Now")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Load this workout and start riding")
            .visible(workout_id.is_some())
            .build();
        let to_start = planned_workout.cloned();
        let on_start_c = Rc::clone(&on_start_workout);
        // Weak: this button is inside the dialog (CLAUDE.md §2.4).
        load_btn.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                let Some(w) = to_start.clone() else { return };
                dialog.close();
                on_start_c(w);
            }
        ));

        // Remove
        let remove_btn = gtk::Button::builder()
            .label("Remove")
            .css_classes(["pill", "destructive-action"])
            .tooltip_text("Remove this workout from the calendar")
            .build();
        let pool_r = pool.clone();
        let rt_r = rt_handle.clone();
        let reload_r = Rc::clone(&reload);
        let remove_btn_for_present = remove_btn.clone();
        // Weak: this button is inside the dialog (CLAUDE.md §2.4).
        remove_btn.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                let pool_c = pool_r.clone();
                let rt_c = rt_r.clone();
                let reload_c = Rc::clone(&reload_r);
                let dialog_c2 = dialog.clone();
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
            }
        ));

        // Ride Route — the route player's equivalent of Load Now.
        if let db::ScheduledItem::Route { id: route_id, .. } = entry.item {
            let ride_btn = gtk::Button::builder()
                .label("Ride Route")
                .css_classes(["pill", "suggested-action"])
                .tooltip_text("Load this route and start riding")
                .build();
            let pool_rr = pool.clone();
            let rt_rr = rt_handle.clone();
            let start_route = Rc::clone(&on_start_route);
            // Weak: this button is inside the dialog (CLAUDE.md §2.4).
            ride_btn.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    let pool_c = pool_rr.clone();
                    let start_route = Rc::clone(&start_route);
                    // Parsing the GPX is file I/O plus a full XML pass, so it
                    // happens off the main thread and the player opens after.
                    crate::ui::spawn_to_main(
                        &rt_rr,
                        async move { load_saved_route(&pool_c, route_id).await },
                        move |res| match res {
                            Ok(route) => {
                                let cb = start_route.borrow().clone();
                                match cb {
                                    Some(cb) => {
                                        dialog.close();
                                        cb(route);
                                    }
                                    None => tracing::error!(
                                        "no route-start callback registered; cannot ride"
                                    ),
                                }
                            }
                            Err(e) => tracing::error!("could not open route {route_id}: {e}"),
                        },
                    );
                }
            ));
            btn_row.append(&ride_btn);
        }

        btn_row.append(&remove_btn);
        btn_row.append(&move_btn);
        btn_row.append(&load_btn);
        content.append(&btn_row);
    }

    dialog.present(Some(parent));
}

/// Ask for a new date for an already-planned entry, and move it there.
///
/// `parent_dialog` is the detail dialog this was opened from; it closes once the
/// move lands so the rider is not left looking at a stale date. It is passed by
/// value rather than captured weakly because the alert is transient and the host
/// releases it on close, which breaks the cycle on its own (CLAUDE.md §2.4).
#[allow(clippy::too_many_arguments)]
fn show_reschedule_dialog(
    parent: &impl IsA<gtk::Widget>,
    entry_id: i64,
    current_date: &str,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    reload: Rc<dyn Fn()>,
    parent_dialog: adw::Dialog,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Reschedule")
        .body("Pick the day to move this session to.")
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("move", "_Move");
    dialog.set_response_appearance("move", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("move"));
    dialog.set_close_response("cancel");

    let calendar = gtk::Calendar::new();
    if let Ok(d) = NaiveDate::parse_from_str(current_date, "%Y-%m-%d") {
        if let Ok(dt) =
            glib::DateTime::from_local(d.year(), d.month() as i32, d.day() as i32, 0, 0, 0.0)
        {
            calendar.select_day(&dt);
        }
    }
    dialog.set_extra_child(Some(&calendar));

    dialog.connect_response(None, move |d, response| {
        if response != "move" {
            return;
        }
        let picked = calendar.date();
        let date = format!(
            "{:04}-{:02}-{:02}",
            picked.year(),
            picked.month(),
            picked.day_of_month()
        );

        let pool_c = pool.clone();
        let reload_c = Rc::clone(&reload);
        let parent_dialog = parent_dialog.clone();
        let d = d.clone();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move { db::reschedule_entry(&pool_c, entry_id, &date).await },
            move |res| {
                match res {
                    Ok(true) => {}
                    // The entry was completed or has since been removed. Say so
                    // rather than closing as though the move had worked.
                    Ok(false) => tracing::warn!("entry {entry_id} was not moved"),
                    Err(e) => tracing::error!("reschedule_entry: {e}"),
                }
                d.close();
                parent_dialog.close();
                reload_c();
            },
        );
    });

    dialog.present(Some(parent));
}

// ── Month label helper ────────────────────────────────────────────────────

// ── Month summary cards ───────────────────────────────────────────────────

// ── Week view ─────────────────────────────────────────────────────────────
