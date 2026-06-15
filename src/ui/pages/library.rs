use adw::prelude::*;
use chrono::Local;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::db;
use crate::data::import::{parse_erg, parse_zwo};
use crate::data::route::Route;
use crate::data::workout::{Workout, WorkoutCategory};
use libshumate::prelude::LocationExt;

/// Self-referential rebuild-callback holder — lets a closure reference itself
/// (so edit/delete callbacks can trigger a list rebuild).
type RebuildHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Athlete fitness context used to tailor per-workout recommendations
/// (CTL/TSB and lower-cased goal keywords). It is loaded asynchronously after
/// the page is built — see CLAUDE.md §2.3 — so it starts neutral and the list
/// is rebuilt once the real data arrives.
#[derive(Default)]
struct FitnessContext {
    ctl: f64,
    tsb: f64,
    goals: Rc<Vec<String>>,
}

/// Draft segment list: `(duration_secs, power_low_pct, power_high_pct, label)`.
type SegmentDraft = Rc<RefCell<Vec<(u32, f32, f32, String)>>>;

pub struct LibraryPage {
    root: gtk::Box,
}

impl LibraryPage {
    #[allow(clippy::too_many_arguments)] // page constructor wiring; grouping deferred
    pub fn new(
        workouts: Vec<Workout>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_start: Rc<dyn Fn(Workout)>,
        calendar_icon: &'static str,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        ftp: u32,
        on_start_route: Rc<dyn Fn(Route)>,
    ) -> (Self, impl Fn() + 'static) {
        // Per-workout recommendations need CTL/TSB and the athlete's goals, all
        // loaded from the database. Rather than block the GLib loop while those
        // queries run (CLAUDE.md §2.3), build the page immediately with a neutral
        // context and rebuild the list once the data arrives (see end of `new`).
        let fitness_ctx: Rc<RefCell<FitnessContext>> =
            Rc::new(RefCell::new(FitnessContext::default()));

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Search bar (revealed by Ctrl+F, dismissed by Escape) ─────────────
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search workouts…")
            .hexpand(true)
            .build();

        let search_bar = gtk::SearchBar::builder()
            .child(&search_entry)
            .show_close_button(true)
            .build();
        search_bar.connect_entry(&search_entry);
        root.append(&search_bar);

        // ── Toolbar row: Import button (right-aligned) ───────────────────────
        let toolbar_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(18)
            .margin_end(18)
            .build();

        let gpx_btn = gtk::Button::builder()
            .label("Load GPX Route")
            .icon_name("map-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Load a GPX file to preview the route and elevation profile")
            .halign(gtk::Align::End)
            .build();

        let import_btn = gtk::Button::builder()
            .label("Import ZWO / ERG")
            .css_classes(["flat"])
            .tooltip_text("Import a ZWO, ERG, or MRC workout file")
            .halign(gtk::Align::End)
            .build();

        let new_workout_btn = gtk::Button::builder()
            .label("New Workout")
            .icon_name("list-add-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Create a new custom workout")
            .halign(gtk::Align::End)
            .build();

        toolbar_row.append(&gtk::Label::builder().hexpand(true).build());
        toolbar_row.append(&new_workout_btn);
        toolbar_row.append(&gpx_btn);
        toolbar_row.append(&import_btn);
        root.append(&toolbar_row);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // ── Help banner ───────────────────────────────────────────────────────
        let help_banner = adw::Banner::builder()
            .title(
                "Click any workout to view its power profile, then start or schedule it \
                 from the detail view.",
            )
            .button_label("Got it")
            .revealed(true)
            .build();
        {
            let banner = help_banner.clone();
            help_banner.connect_button_clicked(move |_| banner.set_revealed(false));
        }
        root.append(&help_banner);

        // ── Category filter chips ────────────────────────────────────────────
        let filter_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .margin_top(12)
            .margin_start(18)
            .margin_end(18)
            .build();

        let filter_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();

        filter_box.append(
            &gtk::Label::builder()
                .label("Filter:")
                .css_classes(["dim-label"])
                .build(),
        );

        // ── Dynamic list container ───────────────────────────────────────────
        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .margin_top(12)
            .build();

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_start(18)
            .margin_end(18)
            .margin_bottom(24)
            .build();

        let list_container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        clamp.set_child(Some(&list_container));
        list_scroll.set_child(Some(&clamp));

        // ── Filter / search state ─────────────────────────────────────────────
        let active_cats: Rc<RefCell<HashSet<WorkoutCategory>>> =
            Rc::new(RefCell::new(HashSet::new()));
        let search_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let workouts_rc: Rc<RefCell<Vec<Workout>>> = Rc::new(RefCell::new(workouts));

        // ── Rebuild closure — clears and repopulates list_container ──────────
        // rebuild_holder lets the closure reference itself (for edit/delete callbacks).
        let rebuild_holder: RebuildHolder = Rc::new(RefCell::new(None));

        let rebuild: Rc<dyn Fn()> = {
            let list_container = list_container.clone();
            let active_cats = Rc::clone(&active_cats);
            let search_text = Rc::clone(&search_text);
            let workouts_rc = Rc::clone(&workouts_rc);
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let on_start = Rc::clone(&on_start);
            let on_toast = Rc::clone(&on_toast);
            let rebuild_holder = Rc::clone(&rebuild_holder);
            let fitness_ctx = Rc::clone(&fitness_ctx);

            Rc::new(move || {
                // Snapshot the current fitness context for this rebuild pass.
                let ctx = fitness_ctx.borrow();
                let ctl = ctx.ctl;
                let tsb = ctx.tsb;
                let goal_descriptions = Rc::clone(&ctx.goals);
                drop(ctx);

                while let Some(child) = list_container.first_child() {
                    list_container.remove(&child);
                }

                let workouts_snapshot = workouts_rc.borrow().clone();
                let active = active_cats.borrow();
                let search = search_text.borrow();
                let search_lower = search.to_lowercase();

                let category_order = [
                    WorkoutCategory::Recovery,
                    WorkoutCategory::Endurance,
                    WorkoutCategory::Tempo,
                    WorkoutCategory::SweetSpot,
                    WorkoutCategory::Threshold,
                    WorkoutCategory::Vo2Max,
                    WorkoutCategory::Anaerobic,
                    WorkoutCategory::Custom,
                ];

                let mut any_visible = false;

                for cat in &category_order {
                    if !active.is_empty() && !active.contains(cat) {
                        continue;
                    }

                    let matching: Vec<&Workout> = workouts_snapshot
                        .iter()
                        .filter(|w| {
                            w.category == *cat
                                && (search_lower.is_empty()
                                    || w.name.to_lowercase().contains(&search_lower))
                        })
                        .collect();

                    if matching.is_empty() {
                        continue;
                    }

                    any_visible = true;

                    list_container.append(
                        &gtk::Label::builder()
                            .label(cat.label())
                            .halign(gtk::Align::Start)
                            .css_classes(["title-4"])
                            .build(),
                    );

                    let group = adw::PreferencesGroup::new();

                    for workout in &matching {
                        let tss = workout.tss as u32;
                        // Difficulty is keyed on peak segment intensity (% FTP), not TSS.
                        // TSS undervalues short high-intensity work: a 6×15 s sprint workout
                        // at 175 % FTP has very low TSS but is physiologically very hard.
                        let peak_pct = workout
                            .segments
                            .iter()
                            .map(|s| s.power_high_pct.max(s.power_low_pct))
                            .fold(0.0f32, f32::max);
                        let (difficulty, diff_class) = if peak_pct >= 130.0 {
                            ("Very Hard", "error")
                        } else if peak_pct >= 110.0 || tss > 100 {
                            ("Hard", "warning")
                        } else if peak_pct >= 88.0 || tss > 50 {
                            ("Moderate", "accent")
                        } else {
                            ("Easy", "success")
                        };
                        let meta = format!(
                            "{} min · TSS {} · {}",
                            workout.duration_secs / 60,
                            tss,
                            workout.category.label()
                        );
                        let subtitle = if workout.description.trim().is_empty() {
                            meta
                        } else {
                            format!("{} — {}", meta, workout.description.trim())
                        };

                        let row = adw::ActionRow::builder()
                            .title(&workout.name)
                            .subtitle(&subtitle)
                            .activatable(true)
                            .build();

                        let diff_badge = gtk::Label::builder()
                            .label(difficulty)
                            .css_classes(["caption", "pill", diff_class])
                            .valign(gtk::Align::Center)
                            .build();
                        row.add_prefix(&diff_badge);

                        let (is_rec, _, _) =
                            workout_fitness_context(workout, ctl, tsb, &goal_descriptions);
                        if is_rec {
                            let star = gtk::Image::builder()
                                .icon_name("starred-symbolic")
                                .css_classes(["success"])
                                .tooltip_text("Recommended based on your current fitness and goals")
                                .valign(gtk::Align::Center)
                                .build();
                            row.add_suffix(&star);
                        }

                        let on_start_detail = Rc::clone(&on_start);
                        let on_toast_detail = Rc::clone(&on_toast);
                        let pool_detail = pool.clone();
                        let rt_detail = rt_handle.clone();
                        let workout_detail = (*workout).clone();
                        let goals_detail = Rc::clone(&goal_descriptions);
                        row.connect_activated(move |row| {
                            let parent = row.root().and_downcast::<gtk::Window>();
                            show_workout_detail(
                                workout_detail.clone(),
                                ftp,
                                ctl,
                                tsb,
                                Rc::clone(&goals_detail),
                                Rc::clone(&on_start_detail),
                                Rc::clone(&on_toast_detail),
                                pool_detail.clone(),
                                rt_detail.clone(),
                                calendar_icon,
                                parent.as_ref(),
                            );
                        });

                        // Edit / delete buttons for custom (user-created) workouts
                        if workout.category == WorkoutCategory::Custom {
                            let edit_btn = gtk::Button::builder()
                                .icon_name("document-edit-symbolic")
                                .tooltip_text("Edit this workout")
                                .css_classes(["flat", "circular"])
                                .valign(gtk::Align::Center)
                                .build();
                            let delete_btn = gtk::Button::builder()
                                .icon_name("user-trash-symbolic")
                                .tooltip_text("Delete this workout")
                                .css_classes(["destructive-action", "flat", "circular"])
                                .valign(gtk::Align::Center)
                                .build();

                            let pool_edit = pool.clone();
                            let rt_edit = rt_handle.clone();
                            let workout_edit = (*workout).clone();
                            let workouts_edit = Rc::clone(&workouts_rc);
                            let rebuild_edit_h = Rc::clone(&rebuild_holder);
                            let on_toast_edit = Rc::clone(&on_toast);
                            edit_btn.connect_clicked(move |btn| {
                                let parent = btn.root().and_downcast::<gtk::Window>();
                                let rebuild_edit = rebuild_edit_h
                                    .borrow()
                                    .as_ref()
                                    .expect("rebuild set")
                                    .clone();
                                show_workout_editor(
                                    parent.as_ref(),
                                    pool_edit.clone(),
                                    rt_edit.clone(),
                                    ftp,
                                    Rc::clone(&workouts_edit),
                                    rebuild_edit,
                                    Rc::clone(&on_toast_edit),
                                    Some(workout_edit.clone()),
                                );
                            });

                            let pool_del = pool.clone();
                            let rt_del = rt_handle.clone();
                            let workout_id_del = workout.id;
                            let workouts_del = Rc::clone(&workouts_rc);
                            let rebuild_del_h = Rc::clone(&rebuild_holder);
                            let on_toast_del = Rc::clone(&on_toast);
                            delete_btn.connect_clicked(move |btn| {
                                let pool_d = pool_del.clone();
                                let rt_d = rt_del.clone();
                                let workouts_d = Rc::clone(&workouts_del);
                                let rebuild_d = rebuild_del_h
                                    .borrow()
                                    .as_ref()
                                    .expect("rebuild set")
                                    .clone();
                                let on_toast_d = Rc::clone(&on_toast_del);
                                crate::ui::widgets::dialog::confirm_destructive(
                                    btn,
                                    "Delete Workout?",
                                    "This workout will be permanently removed.",
                                    "_Delete",
                                    move || {
                                        let pool_d = pool_d.clone();
                                        let workouts_d = Rc::clone(&workouts_d);
                                        let rebuild_d = rebuild_d.clone();
                                        let on_toast_d = Rc::clone(&on_toast_d);
                                        crate::ui::spawn_to_main(
                                            &rt_d,
                                            async move {
                                                db::delete_workout(&pool_d, workout_id_del).await
                                            },
                                            move |res| {
                                                if let Err(e) = res {
                                                    tracing::error!("delete_workout failed: {e}");
                                                    on_toast_d(
                                                        adw::Toast::builder()
                                                            .title("Failed to delete workout")
                                                            .timeout(4)
                                                            .build(),
                                                    );
                                                    return;
                                                }
                                                workouts_d
                                                    .borrow_mut()
                                                    .retain(|w| w.id != workout_id_del);
                                                rebuild_d();
                                            },
                                        );
                                    },
                                );
                            });

                            row.add_suffix(&edit_btn);
                            row.add_suffix(&delete_btn);
                        }

                        group.add(&row);
                    }

                    list_container.append(&group);
                }

                if !any_visible {
                    list_container.append(
                        &adw::StatusPage::builder()
                            .icon_name("folder-open-symbolic")
                            .title("No Workouts")
                            .description("No workouts match your current filters.")
                            .build(),
                    );
                }
            })
        };
        *rebuild_holder.borrow_mut() = Some(Rc::clone(&rebuild));

        // ── GPX Route button handler ─────────────────────────────────────────
        {
            let on_toast_gpx = Rc::clone(&on_toast);
            let on_start_route_gpx = Rc::clone(&on_start_route);
            gpx_btn.connect_clicked(move |btn| {
                let gpx_filter = gtk::FileFilter::new();
                gpx_filter.set_name(Some("GPX files"));
                gpx_filter.add_pattern("*.gpx");
                gpx_filter.add_pattern("*.GPX");
                let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&gpx_filter);

                let dialog = gtk::FileDialog::builder()
                    .title("Open GPX Route")
                    .accept_label("Open")
                    .build();
                dialog.set_filters(Some(&filters));

                let parent = btn.root().and_downcast::<gtk::Window>();
                let parent_for_closure = parent.clone();
                let on_toast_inner = Rc::clone(&on_toast_gpx);
                let on_start_route_inner = Rc::clone(&on_start_route_gpx);
                dialog.open(
                    parent.as_ref(),
                    gtk::gio::Cancellable::NONE,
                    move |result| {
                        let file = match result {
                            Ok(f) => f,
                            Err(_) => return,
                        };
                        let path = match file.path() {
                            Some(p) => p,
                            None => return,
                        };
                        match Route::from_gpx_path(&path) {
                            Ok(route) => {
                                show_route_detail(
                                    route,
                                    parent_for_closure.as_ref(),
                                    Rc::clone(&on_start_route_inner),
                                );
                            }
                            Err(e) => {
                                tracing::error!("Failed to parse GPX: {e}");
                                on_toast_inner(
                                    adw::Toast::builder()
                                        .title("Failed to load GPX route")
                                        .timeout(4)
                                        .build(),
                                );
                            }
                        }
                    },
                );
            });
        }

        // ── Import button handler ────────────────────────────────────────────
        {
            let workouts_for_import = Rc::clone(&workouts_rc);
            let rebuild_for_import = Rc::clone(&rebuild);
            let pool_for_import = pool.clone();
            let rt_for_import = rt_handle.clone();
            let on_toast_import = Rc::clone(&on_toast);

            import_btn.connect_clicked(move |btn| {
                let dialog = gtk::FileDialog::builder()
                    .title("Import Workout File")
                    .build();

                let parent = btn.root().and_downcast::<gtk::Window>();
                let workouts_clone = Rc::clone(&workouts_for_import);
                let rebuild_clone = Rc::clone(&rebuild_for_import);
                let pool_clone = pool_for_import.clone();
                let rt_clone = rt_for_import.clone();
                let on_toast_inner = Rc::clone(&on_toast_import);

                dialog.open(
                    parent.as_ref(),
                    gtk::gio::Cancellable::NONE,
                    move |result| {
                        let show_error = |msg: &str| {
                            on_toast_inner(adw::Toast::builder().title(msg).timeout(4).build());
                        };

                        let file = match result {
                            Ok(f) => f,
                            Err(_) => return,
                        };
                        let path = match file.path() {
                            Some(p) => p,
                            None => return,
                        };
                        let meta = match std::fs::metadata(&path) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::error!("Failed to read import file metadata: {e}");
                                show_error("Could not read the selected file");
                                return;
                            }
                        };
                        if meta.len() > 1_048_576 {
                            tracing::warn!("Import file too large: {} bytes", meta.len());
                            show_error("File is too large (maximum 1 MB)");
                            return;
                        }
                        let ext = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                        let content = match std::fs::read_to_string(&path) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::error!("Failed to read import file: {e}");
                                show_error("Could not read the selected file");
                                return;
                            }
                        };
                        let parsed = match ext.as_str() {
                            "zwo" => parse_zwo(&content),
                            "erg" | "mrc" => parse_erg(&content),
                            _ => {
                                tracing::warn!("Unsupported workout file format: {ext}");
                                show_error("Unsupported file format — use .zwo, .erg, or .mrc");
                                return;
                            }
                        };
                        let mut workout = match parsed {
                            Ok(w) => w,
                            Err(e) => {
                                tracing::error!("Failed to parse workout file: {e}");
                                show_error("Could not parse the workout file");
                                return;
                            }
                        };
                        let on_toast_done = Rc::clone(&on_toast_inner);
                        crate::ui::spawn_to_main(
                            &rt_clone,
                            async move {
                                match db::save_workout(&pool_clone, &workout).await {
                                    Ok(id) => {
                                        workout.id = id;
                                        Ok(workout)
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            },
                            move |res| match res {
                                Ok(workout) => {
                                    workouts_clone.borrow_mut().push(workout);
                                    rebuild_clone();
                                }
                                Err(e) => {
                                    tracing::error!("Failed to save imported workout: {e}");
                                    on_toast_done(
                                        adw::Toast::builder()
                                            .title("Could not save the workout")
                                            .timeout(4)
                                            .build(),
                                    );
                                }
                            },
                        );
                    },
                );
            });
        }

        // ── New Workout button handler ───────────────────────────────────────
        {
            let workouts_new = Rc::clone(&workouts_rc);
            let rebuild_new = Rc::clone(&rebuild);
            let pool_new = pool.clone();
            let rt_new = rt_handle.clone();
            let on_toast_new = Rc::clone(&on_toast);

            new_workout_btn.connect_clicked(move |btn| {
                let parent = btn.root().and_downcast::<gtk::Window>();
                show_workout_editor(
                    parent.as_ref(),
                    pool_new.clone(),
                    rt_new.clone(),
                    ftp,
                    Rc::clone(&workouts_new),
                    Rc::clone(&rebuild_new),
                    Rc::clone(&on_toast_new),
                    None,
                );
            });
        }

        // ── Filter chip buttons ──────────────────────────────────────────────
        for cat in [
            WorkoutCategory::Recovery,
            WorkoutCategory::Endurance,
            WorkoutCategory::Tempo,
            WorkoutCategory::SweetSpot,
            WorkoutCategory::Threshold,
            WorkoutCategory::Vo2Max,
            WorkoutCategory::Anaerobic,
            WorkoutCategory::Custom,
        ] {
            let chip = gtk::ToggleButton::builder()
                .label(cat.label())
                .css_classes(["pill"])
                .build();

            let active_cats_clone = Rc::clone(&active_cats);
            let rebuild_clone = Rc::clone(&rebuild);
            chip.connect_toggled(move |btn| {
                if btn.is_active() {
                    active_cats_clone.borrow_mut().insert(cat);
                } else {
                    active_cats_clone.borrow_mut().remove(&cat);
                }
                rebuild_clone();
            });

            filter_box.append(&chip);
        }

        filter_scroll.set_child(Some(&filter_box));
        root.append(&filter_scroll);

        // ── Difficulty legend ─────────────────────────────────────────────────
        root.append(
            &gtk::Label::builder()
                .label(
                    "Difficulty is based on the peak power target in any segment as a \
                     percentage of your FTP — Easy < 88 % · Moderate 88–110 % · \
                     Hard 110–130 % · Very Hard ≥ 130 %",
                )
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .margin_start(18)
                .margin_end(18)
                .margin_top(6)
                .build(),
        );

        // Ctrl+F reveals the search bar; key events on root propagate into it
        search_bar.set_key_capture_widget(Some(&root));

        // ── Search signal ────────────────────────────────────────────────────
        let search_text_clone = Rc::clone(&search_text);
        let rebuild_search = Rc::clone(&rebuild);
        search_entry.connect_search_changed(move |entry| {
            *search_text_clone.borrow_mut() = entry.text().to_string();
            rebuild_search();
        });

        root.append(&list_scroll);

        rebuild();

        // Load CTL/TSB + goals off the main thread, then refresh recommendations.
        {
            let pool_load = pool.clone();
            let fitness_ctx_load = Rc::clone(&fitness_ctx);
            let rebuild_load = Rc::clone(&rebuild);
            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    let records = db::load_session_records(&pool_load)
                        .await
                        .unwrap_or_default();
                    let intervals_pairs = db::load_intervals_tss_pairs(&pool_load)
                        .await
                        .unwrap_or_default();
                    let goals: Vec<String> = db::load_goals(&pool_load)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|g| g.description.to_lowercase())
                        .collect();
                    (records, intervals_pairs, goals)
                },
                move |(records, intervals_pairs, goals)| {
                    let today = Local::now().date_naive();
                    let (ctl, atl, _) = crate::ui::pages::fitness::compute_load_metrics(
                        &records,
                        &intervals_pairs,
                        ftp,
                        today,
                    );
                    *fitness_ctx_load.borrow_mut() = FitnessContext {
                        ctl,
                        tsb: ctl - atl,
                        goals: Rc::new(goals),
                    };
                    rebuild_load();
                },
            );
        }

        let reload = {
            let rebuild = Rc::clone(&rebuild);
            move || rebuild()
        };

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn show_schedule_dialog(
        parent: &gtk::Button,
        workout_id: i64,
        workout_name: &str,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) {
        let dialog = adw::AlertDialog::builder()
            .heading("Schedule Workout")
            .build();
        dialog.set_body(&format!("Pick a date to schedule \"{}\".", workout_name));

        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("schedule", "_Schedule");
        dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("schedule"));
        dialog.set_close_response("cancel");

        let calendar = gtk::Calendar::new();
        dialog.set_extra_child(Some(&calendar));

        dialog.connect_response(None, move |_, response| {
            if response != "schedule" {
                return;
            }
            let dt = calendar.date();
            let date_str = format!(
                "{:04}-{:02}-{:02}",
                dt.year(),
                dt.month(),
                dt.day_of_month()
            );
            let pool = pool.clone();
            let on_toast = Rc::clone(&on_toast);
            crate::ui::spawn_to_main(
                &rt_handle,
                async move { db::schedule_workout(&pool, workout_id, &date_str).await },
                move |res| match res {
                    Ok(_) => {
                        tracing::info!("Scheduled workout {workout_id}");
                        on_toast(
                            adw::Toast::builder()
                                .title("Workout added to calendar")
                                .timeout(3)
                                .build(),
                        );
                    }
                    Err(e) => {
                        tracing::error!("schedule_workout failed: {e}");
                        on_toast(
                            adw::Toast::builder()
                                .title("Failed to schedule workout")
                                .timeout(4)
                                .build(),
                        );
                    }
                },
            );
        });

        dialog.present(Some(parent));
    }
}

#[allow(clippy::too_many_arguments)] // detail dialog wiring; grouping deferred
fn show_workout_detail(
    workout: Workout,
    ftp: u32,
    ctl: f64,
    tsb: f64,
    goals: Rc<Vec<String>>,
    on_start: Rc<dyn Fn(Workout)>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    calendar_icon: &'static str,
    parent: Option<&gtk::Window>,
) {
    use crate::data::athlete::AthleteProfile;
    use crate::ui::widgets::workout_graph::WorkoutGraph;

    let win = adw::Window::builder()
        .modal(true)
        .title(&workout.name)
        .default_width(480)
        .default_height(580)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // ── Stats ─────────────────────────────────────────────────────────────
    let stats_group = adw::PreferencesGroup::builder().title("Details").build();

    let make_stat_row = |title: &str, value: String| {
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(
            &gtk::Label::builder()
                .label(&value)
                .css_classes(["dim-label", "numeric"])
                .valign(gtk::Align::Center)
                .build(),
        );
        row
    };

    let total_mins = workout.duration_secs / 60;
    let dur_str = if total_mins >= 60 {
        format!("{} h {:02} min", total_mins / 60, total_mins % 60)
    } else {
        format!("{} min", total_mins)
    };
    stats_group.add(&make_stat_row("Duration", dur_str));
    stats_group.add(&make_stat_row("TSS", format!("{}", workout.tss as u32)));
    stats_group.add(&make_stat_row(
        "Category",
        workout.category.label().to_string(),
    ));

    let peak_pct = workout
        .segments
        .iter()
        .map(|s| s.power_high_pct.max(s.power_low_pct))
        .fold(0.0f32, f32::max);
    let tss_val = workout.tss as u32;
    let difficulty = if peak_pct >= 130.0 {
        "Very Hard"
    } else if peak_pct >= 110.0 || tss_val > 100 {
        "Hard"
    } else if peak_pct >= 88.0 || tss_val > 50 {
        "Moderate"
    } else {
        "Easy"
    };

    // Difficulty row with criteria explained in its subtitle
    let diff_row = adw::ActionRow::builder()
        .title("Difficulty")
        .subtitle(
            "Based on peak power as % of FTP — \
             Easy < 88% · Moderate 88–110% · Hard 110–130% · Very Hard ≥ 130%",
        )
        .build();
    diff_row.add_suffix(
        &gtk::Label::builder()
            .label(difficulty)
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );
    stats_group.add(&diff_row);
    inner.append(&stats_group);

    // ── For You ───────────────────────────────────────────────────────────
    let (is_rec, form_text, reason_text) = workout_fitness_context(&workout, ctl, tsb, &goals);
    let fitness_group = adw::PreferencesGroup::builder().title("For You").build();
    let for_you_row = adw::ActionRow::builder()
        .title(&form_text)
        .subtitle(&reason_text)
        .build();
    let for_you_icon = gtk::Image::builder()
        .icon_name(if is_rec {
            "starred-symbolic"
        } else {
            "dialog-information-symbolic"
        })
        .css_classes(if is_rec {
            ["success"].as_slice()
        } else {
            ["dim-label"].as_slice()
        })
        .valign(gtk::Align::Center)
        .build();
    for_you_row.add_prefix(&for_you_icon);
    fitness_group.add(&for_you_row);
    inner.append(&fitness_group);

    // ── Description ───────────────────────────────────────────────────────
    if !workout.description.trim().is_empty() {
        inner.append(
            &gtk::Label::builder()
                .label(workout.description.trim())
                .wrap(true)
                .xalign(0.0)
                .css_classes(["body"])
                .build(),
        );
    }

    // ── Workout graph ─────────────────────────────────────────────────────
    inner.append(
        &gtk::Label::builder()
            .label("Power Profile")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );
    let athlete = AthleteProfile {
        ftp_watts: ftp,
        ..AthleteProfile::default()
    };
    let graph = WorkoutGraph::new(&workout, &athlete);
    let graph_widget = graph.widget().clone();
    graph_widget.set_accessible_role(gtk::AccessibleRole::Img);
    graph_widget.update_property(&[gtk::accessible::Property::Label("Workout power profile")]);
    inner.append(&graph_widget);

    // ── Action buttons ────────────────────────────────────────────────────
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();

    let schedule_btn = gtk::Button::builder()
        .icon_name(calendar_icon)
        .label("Schedule")
        .css_classes(["pill"])
        .tooltip_text("Add this workout to the calendar")
        .build();

    let start_btn = gtk::Button::builder()
        .label("Start Workout")
        .css_classes(["pill", "suggested-action"])
        .tooltip_text("Start this workout now")
        .build();

    let workout_id = workout.id;
    let workout_name = workout.name.clone();
    schedule_btn.connect_clicked(move |btn| {
        LibraryPage::show_schedule_dialog(
            btn,
            workout_id,
            &workout_name,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_toast),
        );
    });

    let workout_for_start = workout.clone();
    let win_for_start = win.clone();
    start_btn.connect_clicked(move |_| {
        on_start(workout_for_start.clone());
        win_for_start.close();
    });

    actions.append(&schedule_btn);
    actions.append(&start_btn);
    inner.append(&actions);

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

fn show_route_detail(
    route: Route,
    parent: Option<&gtk::Window>,
    on_start_route: Rc<dyn Fn(Route)>,
) {
    let win = adw::Window::builder()
        .modal(true)
        .title(&route.name)
        .default_width(480)
        .default_height(620)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // ── Stats ─────────────────────────────────────────────────────────────
    let stats_group = adw::PreferencesGroup::builder()
        .title("Route Stats")
        .build();
    let make_row = |lbl: &str, val: String| {
        let row = adw::ActionRow::builder().title(lbl).build();
        row.add_suffix(
            &gtk::Label::builder()
                .label(&val)
                .css_classes(["dim-label", "numeric"])
                .valign(gtk::Align::Center)
                .build(),
        );
        row
    };

    let dist_km = route.total_distance_m / 1000.0;
    stats_group.add(&make_row("Distance", format!("{dist_km:.2} km")));
    stats_group.add(&make_row(
        "Elevation Gain",
        format!("{:.0} m", route.total_gain_m),
    ));
    stats_group.add(&make_row("Points", route.points.len().to_string()));

    // Estimated duration at 25 km/h
    let est_secs = (route.total_distance_m / (25_000.0 / 3600.0)) as u32;
    let est_min = est_secs / 60;
    stats_group.add(&make_row(
        "Est. Duration (25 km/h)",
        format!("{} h {:02} min", est_min / 60, est_min % 60),
    ));

    // Average gradient
    let avg_grad = if route.total_distance_m > 0.0 {
        route.total_gain_m / route.total_distance_m * 100.0
    } else {
        0.0
    };
    stats_group.add(&make_row("Avg Gradient", format!("{avg_grad:.1}%")));

    inner.append(&stats_group);

    // ── Elevation Profile ─────────────────────────────────────────────────
    let ele_pts: Vec<(f32, f32)> = route
        .points
        .iter()
        .map(|p| (p.distance_m / 1000.0, p.elevation_m))
        .collect();

    let ele_data = Rc::new(ele_pts);
    let ele_chart = gtk::DrawingArea::builder()
        .content_height(100)
        .hexpand(true)
        .build();
    {
        let data = Rc::clone(&ele_data);
        ele_chart.set_draw_func(move |_w, cr, width, height| {
            let pts = &*data;
            if pts.len() < 2 {
                return;
            }
            let w = width as f64;
            let h = height as f64;
            let x_max = pts.last().map(|p| p.0).unwrap_or(1.0) as f64;
            let y_min = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min) as f64;
            let y_max = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max) as f64;
            let y_span = (y_max - y_min).max(1.0);
            let pad_t = 4.0;
            let usable = h - pad_t - 2.0;

            let to_xy = |x: f32, y: f32| -> (f64, f64) {
                (
                    x as f64 / x_max * w,
                    pad_t + (1.0 - (y as f64 - y_min) / y_span) * usable,
                )
            };

            let (x0, y0) = to_xy(pts[0].0, pts[0].1);
            cr.set_source_rgba(1.0, 0.75, 0.20, 0.25);
            cr.move_to(x0, h);
            cr.line_to(x0, y0);
            for p in &pts[1..] {
                let (x, y) = to_xy(p.0, p.1);
                cr.line_to(x, y);
            }
            let (xl, _) = to_xy(pts[pts.len() - 1].0, pts[pts.len() - 1].1);
            cr.line_to(xl, h);
            cr.close_path();
            cr.fill().ok();

            cr.set_source_rgba(1.0, 0.75, 0.20, 0.85);
            cr.set_line_width(1.5);
            cr.move_to(x0, y0);
            for p in &pts[1..] {
                let (x, y) = to_xy(p.0, p.1);
                cr.line_to(x, y);
            }
            cr.stroke().ok();
        });
    }

    inner.append(
        &gtk::Label::builder()
            .label("Elevation Profile")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );
    inner.append(&ele_chart);

    // ── Map ───────────────────────────────────────────────────────────────
    let gps: Vec<(f64, f64)> = route.points.iter().map(|p| (p.lat, p.lng)).collect();
    if gps.len() >= 2 {
        let lat_min = gps.iter().map(|&(la, _)| la).fold(f64::INFINITY, f64::min);
        let lat_max = gps
            .iter()
            .map(|&(la, _)| la)
            .fold(f64::NEG_INFINITY, f64::max);
        let lng_min = gps.iter().map(|&(_, lo)| lo).fold(f64::INFINITY, f64::min);
        let lng_max = gps
            .iter()
            .map(|&(_, lo)| lo)
            .fold(f64::NEG_INFINITY, f64::max);
        let center_lat = (lat_min + lat_max) / 2.0;
        let center_lng = (lng_min + lng_max) / 2.0;
        let max_span = (lat_max - lat_min).max(lng_max - lng_min).max(1e-9);
        let zoom = ((360.0_f64 / max_span).log2() - 1.0).clamp(2.0, 16.0);

        let route_map = libshumate::SimpleMap::new();
        route_map.set_hexpand(true);
        route_map.set_size_request(-1, 220);
        route_map.set_map_source(Some(&libshumate::RasterRenderer::from_url(
            "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
        )));

        if let Some(viewport) = route_map.viewport() {
            viewport.set_location(center_lat, center_lng);
            viewport.set_zoom_level(zoom);
            let path_layer = libshumate::PathLayer::new(&viewport);
            let downsampled = crate::data::streams::ActivityStreams::downsample(&gps, 500);
            for &(lat, lng) in &downsampled {
                path_layer.add_node(&libshumate::Coordinate::new_full(lat, lng));
            }
            path_layer.set_stroke_color(Some(&gtk::gdk::RGBA::new(0.35, 0.60, 1.0, 0.9)));
            path_layer.set_stroke_width(3.0);
            route_map.add_overlay_layer(&path_layer);
        }

        inner.append(
            &gtk::Label::builder()
                .label("Route Map")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        inner.append(&route_map);
    }

    // ── Ride this Route button ────────────────────────────────────────────
    let ride_btn = gtk::Button::builder()
        .label("Ride this Route")
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .tooltip_text("Start an indoor ride following this route's gradient profile")
        .build();
    inner.append(&ride_btn);

    let route_for_ride = route.clone();
    let win_for_ride = win.clone();
    ride_btn.connect_clicked(move |_| {
        on_start_route(route_for_ride.clone());
        win_for_ride.close();
    });

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

/// Open the workout editor dialog. Pass `Some(workout)` to edit an existing workout,
/// or `None` to create a new one.
#[allow(clippy::too_many_arguments)] // editor dialog wiring; grouping deferred
fn show_workout_editor(
    parent: Option<&gtk::Window>,
    pool: sqlx::SqlitePool,
    rt_handle: tokio::runtime::Handle,
    ftp: u32,
    workouts_list: Rc<RefCell<Vec<Workout>>>,
    rebuild: Rc<dyn Fn()>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    existing: Option<Workout>,
) {
    use crate::data::athlete::AthleteProfile;
    use crate::ui::widgets::workout_graph::WorkoutGraph;

    // Extract what we need up-front so `existing` doesn't need to reach into closures.
    let existing_id: Option<i64> = existing.as_ref().map(|w| w.id);
    let initial_name = existing
        .as_ref()
        .map(|w| w.name.as_str())
        .unwrap_or("Custom Workout")
        .to_string();
    let initial_desc = existing
        .as_ref()
        .map(|w| w.description.as_str())
        .unwrap_or("")
        .to_string();
    let initial_segments: Vec<(u32, f32, f32, String)> = existing
        .as_ref()
        .map(|w| {
            w.segments
                .iter()
                .map(|s| {
                    (
                        s.duration_secs,
                        s.power_low_pct,
                        s.power_high_pct,
                        s.label.clone().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                (600, 40.0, 60.0, "Warm-up".to_string()),
                (1800, 75.0, 75.0, "Main Set".to_string()),
                (300, 40.0, 40.0, "Cool-down".to_string()),
            ]
        });

    let win = adw::Window::builder()
        .modal(true)
        .title(if existing_id.is_some() {
            "Edit Workout"
        } else {
            "New Workout"
        })
        .default_width(520)
        .default_height(680)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .css_classes(["flat"])
        .tooltip_text("Discard and close")
        .build();
    let save_btn = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .tooltip_text("Save this workout to the library")
        .build();
    header.pack_start(&cancel_btn);
    header.pack_end(&save_btn);
    toolbar_view.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(480)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // ── Name / description ────────────────────────────────────────────────
    let meta_group = adw::PreferencesGroup::builder().title("Workout").build();

    let name_row = adw::EntryRow::builder().title("Name").build();
    name_row.set_text(&initial_name);
    meta_group.add(&name_row);

    let desc_row = adw::EntryRow::builder().title("Description").build();
    desc_row.set_text(&initial_desc);
    meta_group.add(&desc_row);

    inner.append(&meta_group);

    // ── Segment list ──────────────────────────────────────────────────────
    let segments_group = adw::PreferencesGroup::builder().title("Segments").build();
    inner.append(&segments_group);

    // Draft segment state — list of (duration_secs, power_low_pct, power_high_pct, label)
    let draft: SegmentDraft = Rc::new(RefCell::new(initial_segments));

    let preview_athlete = AthleteProfile {
        ftp_watts: ftp,
        ..AthleteProfile::default()
    };
    let preview_workout = build_draft_workout(&draft.borrow(), "Custom Workout");

    // Live preview graph — defined early so rebuild_segments can update it.
    let graph_for_update: Rc<WorkoutGraph> =
        Rc::new(WorkoutGraph::new(&preview_workout, &preview_athlete));

    // Track added expander rows so we can remove only our rows from PreferencesGroup.
    let added_rows: Rc<RefCell<Vec<adw::ExpanderRow>>> = Rc::new(RefCell::new(Vec::new()));

    // Self-referential holder so delete and DnD drop callbacks can call rebuild_segments.
    let rebuild_segs_holder: RebuildHolder = Rc::new(RefCell::new(None));

    let rebuild_segments: Rc<dyn Fn()> = {
        let draft = Rc::clone(&draft);
        let added_rows = Rc::clone(&added_rows);
        let segments_group = segments_group.clone();
        let graph_rc = Rc::clone(&graph_for_update);
        let name_row = name_row.clone();
        let rebuild_segs_holder = Rc::clone(&rebuild_segs_holder);

        Rc::new(move || {
            for row in added_rows.borrow().iter() {
                segments_group.remove(row);
            }
            added_rows.borrow_mut().clear();

            let segs = draft.borrow().clone();
            for (idx, (dur, lo, hi, label)) in segs.iter().enumerate() {
                let is_ramp = (hi - lo).abs() > 0.5;
                let title = if is_ramp {
                    format!(
                        "{} min · {}→{}% FTP{}",
                        dur / 60,
                        lo.round() as u32,
                        hi.round() as u32,
                        if label.is_empty() {
                            String::new()
                        } else {
                            format!(" — {label}")
                        }
                    )
                } else {
                    format!(
                        "{} min · {}% FTP{}",
                        dur / 60,
                        lo.round() as u32,
                        if label.is_empty() {
                            String::new()
                        } else {
                            format!(" — {label}")
                        }
                    )
                };

                let expander = adw::ExpanderRow::builder().title(&title).build();

                // Duration
                let dur_adj = gtk::Adjustment::new(*dur as f64, 10.0, 7200.0, 10.0, 60.0, 0.0);
                let dur_row = adw::SpinRow::new(Some(&dur_adj), 10.0, 0);
                dur_row.set_title("Duration");
                dur_row.set_subtitle("Seconds");
                expander.add_row(&dur_row);

                // Power low %
                let lo_adj = gtk::Adjustment::new(*lo as f64, 10.0, 200.0, 1.0, 5.0, 0.0);
                let lo_row = adw::SpinRow::new(Some(&lo_adj), 1.0, 0);
                lo_row.set_title("Power");
                lo_row.set_subtitle("% of FTP (start of segment)");
                expander.add_row(&lo_row);

                // Power high % (only relevant for ramps)
                let hi_adj = gtk::Adjustment::new(*hi as f64, 10.0, 200.0, 1.0, 5.0, 0.0);
                let hi_row = adw::SpinRow::new(Some(&hi_adj), 1.0, 0);
                hi_row.set_title("Power (end)");
                hi_row.set_subtitle("% of FTP (end of segment — set equal to start for steady)");
                expander.add_row(&hi_row);

                // Label
                let lbl_row = adw::EntryRow::builder().title("Label").build();
                lbl_row.set_text(label);
                expander.add_row(&lbl_row);

                // Delete button
                let del_btn = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Remove this segment")
                    .css_classes(["destructive-action", "flat", "circular"])
                    .valign(gtk::Align::Center)
                    .build();
                expander.add_suffix(&del_btn);

                // Drag handle — lets the user grab and reorder the segment.
                let drag_handle = gtk::Image::builder()
                    .icon_name("list-drag-handle-symbolic")
                    .tooltip_text("Drag to reorder")
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label"])
                    .build();
                expander.add_prefix(&drag_handle);

                // DragSource on the handle — broadcasts the source row index.
                let drag_source = gtk::DragSource::new();
                drag_source.set_actions(gtk::gdk::DragAction::MOVE);
                let src_idx_capture = idx as u32;
                drag_source.connect_prepare(move |_, _, _| {
                    Some(gtk::gdk::ContentProvider::for_value(
                        &src_idx_capture.to_value(),
                    ))
                });
                drag_handle.add_controller(drag_source);

                // DropTarget on the expander row — moves the segment and rebuilds.
                let drop_target =
                    gtk::DropTarget::new(u32::static_type(), gtk::gdk::DragAction::MOVE);
                {
                    let draft_dnd = Rc::clone(&draft);
                    let rebuild_dnd = Rc::clone(&rebuild_segs_holder);
                    let target_idx = idx;
                    drop_target.connect_drop(move |_, value, _, _| {
                        let src_idx = match value.get::<u32>() {
                            Ok(v) => v as usize,
                            Err(_) => return false,
                        };
                        if src_idx == target_idx {
                            return false;
                        }
                        let mut d = draft_dnd.borrow_mut();
                        if src_idx < d.len() {
                            let seg = d.remove(src_idx);
                            let insert_at = if src_idx < target_idx {
                                target_idx.saturating_sub(1)
                            } else {
                                target_idx
                            };
                            let cap = d.len();
                            d.insert(insert_at.min(cap), seg);
                            drop(d);
                            let rebuild = rebuild_dnd
                                .borrow()
                                .as_ref()
                                .expect("rebuild_segs set")
                                .clone();
                            rebuild();
                        }
                        true
                    });
                }
                expander.add_controller(drop_target);

                // Wire spin/entry changes to update draft and preview graph
                {
                    let draft_c = Rc::clone(&draft);
                    let graph = Rc::clone(&graph_rc);
                    let nr = name_row.clone();
                    let i = idx;
                    dur_row.connect_value_notify(move |row| {
                        if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                            seg.0 = row.value() as u32;
                        }
                        let name = nr.text().to_string();
                        let w = build_draft_workout(&draft_c.borrow(), &name);
                        graph.set_workout(&w);
                    });
                }
                {
                    let draft_c = Rc::clone(&draft);
                    let graph = Rc::clone(&graph_rc);
                    let nr = name_row.clone();
                    let i = idx;
                    lo_row.connect_value_notify(move |row| {
                        if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                            seg.1 = row.value() as f32;
                        }
                        let name = nr.text().to_string();
                        let w = build_draft_workout(&draft_c.borrow(), &name);
                        graph.set_workout(&w);
                    });
                }
                {
                    let draft_c = Rc::clone(&draft);
                    let graph = Rc::clone(&graph_rc);
                    let nr = name_row.clone();
                    let i = idx;
                    hi_row.connect_value_notify(move |row| {
                        if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                            seg.2 = row.value() as f32;
                        }
                        let name = nr.text().to_string();
                        let w = build_draft_workout(&draft_c.borrow(), &name);
                        graph.set_workout(&w);
                    });
                }
                {
                    let draft_c = Rc::clone(&draft);
                    let i = idx;
                    lbl_row.connect_changed(move |row| {
                        if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                            seg.3 = row.text().to_string();
                        }
                    });
                }

                segments_group.add(&expander);
                added_rows.borrow_mut().push(expander);

                // Delete callback — removes the segment and triggers a full rebuild.
                let del_idx = idx;
                let draft_del = Rc::clone(&draft);
                let rebuild_del = Rc::clone(&rebuild_segs_holder);
                del_btn.connect_clicked(move |_| {
                    let mut d = draft_del.borrow_mut();
                    if d.len() > 1 {
                        d.remove(del_idx);
                        drop(d);
                        let rebuild = rebuild_del
                            .borrow()
                            .as_ref()
                            .expect("rebuild_segs set")
                            .clone();
                        rebuild();
                    }
                });
            }

            // Push updated workout data into the graph (also queues a redraw).
            let name = name_row.text().to_string();
            let w = build_draft_workout(&segs, &name);
            graph_rc.set_workout(&w);
        })
    };

    // Seal the holder so delete/DnD callbacks can resolve it.
    *rebuild_segs_holder.borrow_mut() = Some(Rc::clone(&rebuild_segments));

    rebuild_segments();

    // ── Add Segment button ────────────────────────────────────────────────
    let add_seg_btn = gtk::Button::builder()
        .label("Add Segment")
        .icon_name("list-add-symbolic")
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .tooltip_text("Add a new segment to the workout")
        .build();

    {
        let draft_add = Rc::clone(&draft);
        let rebuild_add = Rc::clone(&rebuild_segments);
        add_seg_btn.connect_clicked(move |_| {
            draft_add
                .borrow_mut()
                .push((300, 75.0, 75.0, String::new()));
            rebuild_add();
        });
    }
    inner.append(&add_seg_btn);

    // ── Live preview graph ────────────────────────────────────────────────
    inner.append(
        &gtk::Label::builder()
            .label("Preview")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );

    // Append the preview graph widget and wire name changes to update it.
    let graph_inner_widget = graph_for_update.widget().clone();
    graph_inner_widget.set_accessible_role(gtk::AccessibleRole::Img);
    graph_inner_widget
        .update_property(&[gtk::accessible::Property::Label("Workout preview graph")]);
    inner.append(&graph_inner_widget);

    {
        let draft_g = Rc::clone(&draft);
        let graph_g = Rc::clone(&graph_for_update);
        name_row.connect_changed(move |row| {
            let name = row.text().to_string();
            let w = build_draft_workout(&draft_g.borrow(), &name);
            graph_g.set_workout(&w);
        });
    }

    // ── Cancel / Save ─────────────────────────────────────────────────────
    let win_cancel = win.clone();
    cancel_btn.connect_clicked(move |_| win_cancel.close());

    {
        let draft_save = Rc::clone(&draft);
        let name_save = name_row.clone();
        let desc_save = desc_row.clone();
        let pool_save = pool;
        let rt_save = rt_handle;
        let workouts_save = workouts_list;
        let rebuild_save = rebuild;
        let on_toast_save = on_toast;
        let win_save = win.clone();
        // existing_id is Copy (Option<i64>), captured directly by the move closure

        save_btn.connect_clicked(move |_| {
            let name = name_save.text().trim().to_string();
            if name.is_empty() {
                on_toast_save(
                    adw::Toast::builder()
                        .title("Workout name is required")
                        .timeout(3)
                        .build(),
                );
                return;
            }
            let segs = draft_save.borrow().clone();
            if segs.is_empty() {
                on_toast_save(
                    adw::Toast::builder()
                        .title("Add at least one segment")
                        .timeout(3)
                        .build(),
                );
                return;
            }
            let desc = desc_save.text().trim().to_string();
            let mut workout = build_draft_workout(&segs, &name);
            workout.description = desc;

            // Clone per-invocation state for the async result handler (this closure is Fn).
            let pool_save = pool_save.clone();
            let workouts_save = Rc::clone(&workouts_save);
            let rebuild_save = rebuild_save.clone();
            let on_toast_save = Rc::clone(&on_toast_save);
            let win_save = win_save.clone();

            if let Some(id) = existing_id {
                workout.id = id;
                crate::ui::spawn_to_main(
                    &rt_save,
                    async move {
                        match db::update_workout(&pool_save, &workout).await {
                            Ok(()) => Ok(workout),
                            Err(e) => Err(e.to_string()),
                        }
                    },
                    move |res| match res {
                        Ok(workout) => {
                            let mut wl = workouts_save.borrow_mut();
                            if let Some(pos) = wl.iter().position(|w| w.id == id) {
                                wl[pos] = workout;
                            }
                            drop(wl);
                            rebuild_save();
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Workout updated")
                                    .timeout(3)
                                    .build(),
                            );
                            win_save.close();
                        }
                        Err(e) => {
                            tracing::error!("update_workout failed: {e}");
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Failed to update workout")
                                    .timeout(4)
                                    .build(),
                            );
                        }
                    },
                );
            } else {
                crate::ui::spawn_to_main(
                    &rt_save,
                    async move {
                        match db::save_workout(&pool_save, &workout).await {
                            Ok(id) => {
                                workout.id = id;
                                Ok(workout)
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    },
                    move |res| match res {
                        Ok(workout) => {
                            workouts_save.borrow_mut().push(workout);
                            rebuild_save();
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Workout saved")
                                    .timeout(3)
                                    .build(),
                            );
                            win_save.close();
                        }
                        Err(e) => {
                            tracing::error!("save_workout failed: {e}");
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Failed to save workout")
                                    .timeout(4)
                                    .build(),
                            );
                        }
                    },
                );
            }
        });
    }

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

/// Returns `(is_recommended, form_label, reason)` for the given workout.
///
/// `is_recommended` drives the star badge in the list row and the detail dialog.
/// `form_label` describes current fitness state (e.g. "Fresh (form +18)").
/// `reason` explains the recommendation or gives a tip.
fn workout_fitness_context(
    workout: &Workout,
    ctl: f64,
    tsb: f64,
    goals: &[String],
) -> (bool, String, String) {
    if ctl < 1.0 {
        return (
            false,
            "No training data yet".into(),
            "Record a few sessions to unlock personalised recommendations.".into(),
        );
    }

    let form_text = if tsb > 15.0 {
        format!("Fresh (form {:+.0})", tsb)
    } else if tsb < -10.0 {
        format!("Fatigued (form {:+.0})", tsb)
    } else {
        format!("Normal (form {:+.0})", tsb)
    };

    // Fatigue overrides goal-based matching — recovery first
    if tsb < -10.0 {
        let rec = matches!(
            workout.category,
            WorkoutCategory::Recovery | WorkoutCategory::Endurance
        );
        return (
            rec,
            form_text,
            if rec {
                "Easy aerobic work is the most productive choice when you're this fatigued.".into()
            } else {
                "You're carrying significant fatigue — a recovery or easy endurance session would serve you better right now.".into()
            },
        );
    }

    // Goal keyword matching
    let goals_text = goals.join(" ");
    let has = |keywords: &[&str]| keywords.iter().any(|kw| goals_text.contains(kw));

    let wants_base = has(&["base", "aerobic", "zone 2", "endurance", "foundation"]);
    let wants_ftp = has(&["ftp", "threshold", "time trial", "tt"]);
    let wants_race = has(&["race", "event", "competition", "racing"]);
    let wants_power = has(&["power", "sprint", "vo2", "climbing", "intervals"]);

    if wants_base {
        let rec = matches!(
            workout.category,
            WorkoutCategory::Recovery | WorkoutCategory::Endurance | WorkoutCategory::Tempo
        );
        return (
            rec,
            form_text,
            if rec {
                "Aligns with your aerobic base goal — Z1–Z3 work builds the engine.".into()
            } else {
                format!(
                    "Your goal targets aerobic base; {} work is above the ideal intensity range for base building.",
                    workout.category.label()
                )
            },
        );
    }

    if wants_ftp || wants_race {
        let rec = matches!(
            workout.category,
            WorkoutCategory::SweetSpot | WorkoutCategory::Threshold | WorkoutCategory::Vo2Max
        );
        return (
            rec,
            form_text,
            if rec {
                if wants_race {
                    "Solid race-preparation work at the right intensity.".into()
                } else {
                    "Directly targets the FTP gains in your goal.".into()
                }
            } else {
                "Your goal calls for threshold-range work, but any training contributes.".into()
            },
        );
    }

    if wants_power {
        let rec = matches!(
            workout.category,
            WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic | WorkoutCategory::Threshold
        );
        return (
            rec,
            form_text,
            if rec {
                "Targets the power output in your goal.".into()
            } else {
                "Your power goal favours high-intensity work, though a broad base helps too.".into()
            },
        );
    }

    // No goals — use freshness
    if tsb > 15.0 {
        let rec = matches!(
            workout.category,
            WorkoutCategory::Threshold | WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic
        );
        return (
            rec,
            form_text,
            if rec {
                "You're well rested — a great time for a quality hard session.".into()
            } else {
                "You're fresh. Any training works; your form suits hard efforts particularly well."
                    .into()
            },
        );
    }

    (
        false,
        form_text,
        "Add a training goal in the Coaching tab to get targeted recommendations.".into(),
    )
}

/// Build a `Workout` from the editor's draft tuple list.
fn build_draft_workout(segs: &[(u32, f32, f32, String)], name: &str) -> Workout {
    use crate::data::workout::{Segment, WorkoutCategory};
    let segments: Vec<Segment> = segs
        .iter()
        .map(|(dur, lo, hi, lbl)| Segment {
            duration_secs: *dur,
            power_low_pct: *lo,
            power_high_pct: *hi,
            label: if lbl.is_empty() {
                None
            } else {
                Some(lbl.clone())
            },
            cadence_target: None,
        })
        .collect();
    let duration_secs: u32 = segments.iter().map(|s| s.duration_secs).sum();
    let tss: f32 = segments
        .iter()
        .map(|s| {
            let mid = (s.power_low_pct + s.power_high_pct) / 2.0;
            let if_ = mid / 100.0;
            (s.duration_secs as f32 / 3600.0) * if_ * if_ * 100.0
        })
        .sum();
    Workout {
        id: 0,
        name: name.to_string(),
        description: String::new(),
        duration_secs,
        tss,
        category: WorkoutCategory::Custom,
        segments,
    }
}
