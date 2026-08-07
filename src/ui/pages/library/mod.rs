//! The Library page: every workout and route the rider can start.
//!
//! The list rebuilds from scratch whenever the filters, the search text, or the
//! workouts themselves change, so there is one path that decides what is on
//! screen rather than several trying to patch it.

mod detail;
mod editor;
mod routes;
mod workout_list;

use adw::prelude::*;
use chrono::Local;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk::glib;

use crate::data::athlete::AthleteProfile;
use crate::data::db;
use crate::data::import::{parse_erg, parse_zwo};
use crate::data::route::Route;
use crate::data::workout::{Workout, WorkoutCategory};
use crate::ui::widgets::zone_color::{category_zone_rgb, zone_swatch};

use detail::show_route_detail;
use editor::show_workout_editor;
use routes::save_route_to_library;
use workout_list::{RowContext, CATEGORY_ORDER};

/// The largest workout file worth reading — no legitimate .zwo or .erg is
/// anywhere near this, and the whole file is read into memory.
const MAX_IMPORT_BYTES: u64 = 1_048_576;

/// Self-referential rebuild-callback holder — lets a closure reference itself,
/// so edit and delete callbacks can trigger a list rebuild.
pub type RebuildHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Draft segment list: `(duration_secs, power_low_pct, power_high_pct, label)`.
pub type SegmentDraft = Rc<RefCell<Vec<(u32, f32, f32, String)>>>;

/// Athlete fitness context used to tailor per-workout recommendations
/// (CTL/TSB and lower-cased goal keywords). It is loaded asynchronously after
/// the page is built — see CLAUDE.md §2.3 — so it starts neutral and the list
/// is rebuilt once the real data arrives.
#[derive(Default)]
pub struct FitnessContext {
    pub ctl: f64,
    pub tsb: f64,
    pub goals: Rc<Vec<String>>,
}

/// The same data as [`FitnessContext`], but owned outright so it can cross
/// threads — `FitnessContext` holds an `Rc`, which cannot.
struct FitnessSnapshot {
    ctl: f64,
    tsb: f64,
    goals: Vec<String>,
}

/// Load the fitness context the per-workout recommendations are judged against.
async fn load_fitness_context(pool: &SqlitePool, ftp: u32) -> anyhow::Result<FitnessSnapshot> {
    let records = db::load_session_summaries(pool).await?;
    let intervals_pairs = db::load_intervals_tss_pairs(pool).await?;
    // Lower-cased once here — `workout_fit` matches goals as plain substrings.
    let goals: Vec<String> = db::load_goals(pool)
        .await?
        .into_iter()
        .map(|g| g.description.to_lowercase())
        .collect();

    let metrics = crate::training::fitness::compute_load_metrics(
        &records,
        &intervals_pairs,
        ftp,
        Local::now().date_naive(),
    );
    Ok(FitnessSnapshot {
        ctl: metrics.ctl,
        tsb: metrics.tsb(),
        goals,
    })
}

pub struct LibraryPage {
    root: gtk::Box,
}

impl LibraryPage {
    #[allow(clippy::too_many_arguments)] // page constructor wiring
    pub fn new(
        workouts: Vec<Workout>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_start: Rc<dyn Fn(Workout)>,
        calendar_icon: &'static str,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        athlete: Rc<RefCell<AthleteProfile>>,
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

        let Toolbar {
            search_entry,
            search_bar,
            new_workout_btn,
            import_btn,
            gpx_btn,
        } = build_toolbar(&root);

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

        // ── Containers ───────────────────────────────────────────────────────
        let list_container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        // Saved GPX routes sit above the workouts, in their own container so the
        // category filters and the search leave them alone.
        let routes_container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        let library_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        library_column.append(&routes_container);
        library_column.append(&list_container);

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_start(18)
            .margin_end(18)
            .margin_bottom(24)
            .child(&library_column)
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .margin_top(12)
            .child(&clamp)
            .build();

        // ── Filter / search state ────────────────────────────────────────────
        let active_cats: Rc<RefCell<HashSet<WorkoutCategory>>> =
            Rc::new(RefCell::new(HashSet::new()));
        let search_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let workouts_rc: Rc<RefCell<Vec<Workout>>> = Rc::new(RefCell::new(workouts));
        // Chip references for the empty state's "Clear Filters" action.
        let filter_chips: Rc<RefCell<Vec<gtk::ToggleButton>>> = Rc::new(RefCell::new(Vec::new()));

        let reload_routes = routes::reload_closure(
            routes_container,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_start_route),
            Rc::clone(&on_toast),
        );
        reload_routes();

        // ── Rebuild ──────────────────────────────────────────────────────────
        let rebuild_holder: RebuildHolder = Rc::new(RefCell::new(None));
        let row_ctx = Rc::new(RowContext {
            pool: pool.clone(),
            rt_handle: rt_handle.clone(),
            on_start,
            on_toast: Rc::clone(&on_toast),
            workouts: Rc::clone(&workouts_rc),
            rebuild: Rc::clone(&rebuild_holder),
            athlete: Rc::clone(&athlete),
            calendar_icon,
        });

        let rebuild: Rc<dyn Fn()> = {
            let list_container = list_container.clone();
            let active_cats = Rc::clone(&active_cats);
            let search_text = Rc::clone(&search_text);
            let workouts_rc = Rc::clone(&workouts_rc);
            let fitness_ctx = Rc::clone(&fitness_ctx);
            let filter_chips = Rc::clone(&filter_chips);
            let search_entry = search_entry.clone();
            let row_ctx = Rc::clone(&row_ctx);

            Rc::new(move || {
                // Every thumbnail and target wattage is scaled by FTP, so read it
                // per rebuild rather than capturing it once.
                let ftp = row_ctx.athlete.borrow().ftp_watts;
                let fitness = fitness_ctx.borrow();
                let workouts = workouts_rc.borrow();
                let active = active_cats.borrow();
                let search_lower = search_text.borrow().to_lowercase();

                while let Some(child) = list_container.first_child() {
                    list_container.remove(&child);
                }

                let mut any_visible = false;
                for category in CATEGORY_ORDER {
                    let matching: Vec<&Workout> = workouts
                        .iter()
                        .filter(|w| workout_list::matches(w, category, &active, &search_lower))
                        .collect();
                    if matching.is_empty() {
                        continue;
                    }
                    any_visible = true;

                    // Category heading with its zone-colour swatch.
                    let heading = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(6)
                        .build();
                    if let Some(rgb) = category_zone_rgb(&category) {
                        heading.append(&zone_swatch(rgb));
                    }
                    heading.append(
                        &gtk::Label::builder()
                            .label(category.label())
                            .halign(gtk::Align::Start)
                            .css_classes(["title-4"])
                            .build(),
                    );
                    list_container.append(&heading);

                    let group = adw::PreferencesGroup::new();
                    for workout in matching {
                        group.add(&workout_list::build_row(workout, ftp, &fitness, &row_ctx));
                    }
                    list_container.append(&group);
                }

                if !any_visible {
                    list_container.append(&workout_list::empty_state(
                        Rc::clone(&filter_chips),
                        search_entry.clone(),
                    ));
                }
            })
        };
        *rebuild_holder.borrow_mut() = Some(Rc::clone(&rebuild));

        // ── GPX Route button handler ─────────────────────────────────────────
        connect_gpx(
            &gpx_btn,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_start_route),
            Rc::clone(&on_toast),
            Rc::clone(&reload_routes),
        );
        connect_import(
            &import_btn,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&workouts_rc),
            Rc::clone(&rebuild),
            Rc::clone(&on_toast),
        );

        // ── New Workout button handler ───────────────────────────────────────
        {
            let workouts_new = Rc::clone(&workouts_rc);
            let rebuild_new = Rc::clone(&rebuild);
            let pool_new = pool.clone();
            let rt_new = rt_handle.clone();
            let on_toast_new = Rc::clone(&on_toast);
            let athlete_new = Rc::clone(&athlete);

            new_workout_btn.connect_clicked(move |btn| {
                let parent = btn.root().and_downcast::<gtk::Window>();
                show_workout_editor(
                    parent.as_ref(),
                    pool_new.clone(),
                    rt_new.clone(),
                    athlete_new.borrow().ftp_watts,
                    Rc::clone(&workouts_new),
                    Rc::clone(&rebuild_new),
                    Rc::clone(&on_toast_new),
                    None,
                );
            });
        }

        // ── Filter chips ─────────────────────────────────────────────────────
        for category in CATEGORY_ORDER {
            let chip = gtk::ToggleButton::builder()
                .label(category.label())
                .css_classes(["pill"])
                .build();

            let active_cats = Rc::clone(&active_cats);
            let rebuild = Rc::clone(&rebuild);
            chip.connect_toggled(move |btn| {
                if btn.is_active() {
                    active_cats.borrow_mut().insert(category);
                } else {
                    active_cats.borrow_mut().remove(&category);
                }
                rebuild();
            });

            filter_chips.borrow_mut().push(chip.clone());
            filter_box.append(&chip);
        }

        filter_scroll.set_child(Some(&filter_box));
        root.append(&filter_scroll);

        // Typing a letter anywhere on the page starts a search; Escape dismisses
        // it. This is all `set_key_capture_widget` does — a modifier combo like
        // Ctrl+F never reaches the search bar, so it is bound separately below.
        search_bar.set_key_capture_widget(Some(&root));
        connect_search_shortcut(&root, &search_bar, &search_entry);

        {
            let search_text = Rc::clone(&search_text);
            let rebuild = Rc::clone(&rebuild);
            search_entry.connect_search_changed(move |entry| {
                *search_text.borrow_mut() = entry.text().to_string();
                rebuild();
            });
        }

        root.append(&list_scroll);
        rebuild();

        // Load CTL/TSB + goals off the main thread, then refresh recommendations.
        {
            let pool_load = pool.clone();
            let fitness_ctx = Rc::clone(&fitness_ctx);
            let rebuild = Rc::clone(&rebuild);
            let ftp = athlete.borrow().ftp_watts;
            crate::ui::spawn_to_main(
                &rt_handle,
                async move { load_fitness_context(&pool_load, ftp).await },
                move |result| {
                    // On failure the page keeps its neutral context, which shows
                    // "No training data yet" rather than a confident but wrong
                    // recommendation. No toast: the list itself is fine, only the
                    // per-workout advice is missing.
                    let snapshot = match result {
                        Ok(snapshot) => snapshot,
                        Err(e) => {
                            tracing::error!("Could not load your fitness context: {e}");
                            return;
                        }
                    };
                    *fitness_ctx.borrow_mut() = FitnessContext {
                        ctl: snapshot.ctl,
                        tsb: snapshot.tsb,
                        goals: Rc::new(snapshot.goals),
                    };
                    rebuild();
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
}

/// The widgets the page's toolbar hands back to its caller.
struct Toolbar {
    search_entry: gtk::SearchEntry,
    search_bar: gtk::SearchBar,
    new_workout_btn: gtk::Button,
    import_btn: gtk::Button,
    gpx_btn: gtk::Button,
}

/// Build the search bar and toolbar row, appending both to `root`.
fn build_toolbar(root: &gtk::Box) -> Toolbar {
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

    // ── Toolbar row: search · [spacer] · New Workout · more menu ─────────
    let toolbar_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(18)
        .margin_end(18)
        .build();

    // Visible entry point for search — Ctrl+F works too, but a hidden
    // shortcut shouldn't be the only way in.
    let search_toggle = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Search workouts (Ctrl+F)")
        .build();
    search_bar
        .bind_property("search-mode-enabled", &search_toggle, "active")
        .bidirectional()
        .sync_create()
        .build();

    // Creating a workout is this page's primary act.
    let new_workout_btn = gtk::Button::builder()
        .css_classes(["suggested-action"])
        .tooltip_text("Create a new custom workout")
        .build();
    new_workout_btn.set_child(Some(
        &adw::ButtonContent::builder()
            .icon_name("list-add-symbolic")
            .label("New Workout")
            .build(),
    ));

    // Rare imports live behind one menu, as on the Calendar.
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
    let import_btn = menu_item(
        "document-open-symbolic",
        "Import ZWO / ERG",
        "Import a ZWO, ERG, or MRC workout file",
    );
    let gpx_btn = menu_item(
        "map-symbolic",
        "Load GPX route",
        "Load a GPX file to preview the route and elevation profile",
    );
    let menu_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    menu_box.append(&import_btn);
    menu_box.append(&gpx_btn);
    let more_popover = gtk::Popover::builder().child(&menu_box).build();
    for btn in [&import_btn, &gpx_btn] {
        let popover = more_popover.clone();
        btn.connect_clicked(move |_| popover.popdown());
    }
    let more_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Import workouts and routes")
        .css_classes(["flat"])
        .popover(&more_popover)
        .build();

    toolbar_row.append(&search_toggle);
    toolbar_row.append(&gtk::Label::builder().hexpand(true).build());
    toolbar_row.append(&new_workout_btn);
    toolbar_row.append(&more_btn);
    root.append(&toolbar_row);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    Toolbar {
        search_entry,
        search_bar,
        new_workout_btn,
        import_btn,
        gpx_btn,
    }
}

/// Load a GPX file: keep a copy in the library, then show the route.
fn connect_gpx(
    gpx_btn: &gtk::Button,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_start_route: Rc<dyn Fn(Route)>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    reload_routes: Rc<dyn Fn()>,
) {
    let on_toast_gpx = Rc::clone(&on_toast);
    let on_start_route_gpx = Rc::clone(&on_start_route);
    let pool_gpx = pool.clone();
    let rt_gpx = rt_handle.clone();
    let reload_routes_gpx = Rc::clone(&reload_routes);
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
        let pool_gpx_inner = pool_gpx.clone();
        let rt_gpx_inner = rt_gpx.clone();
        let reload_routes_inner = Rc::clone(&reload_routes_gpx);
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
                        // Keep the route: copy the GPX into the library so
                        // it can be ridden again without finding the file.
                        save_route_to_library(
                            &path,
                            &route,
                            pool_gpx_inner.clone(),
                            rt_gpx_inner.clone(),
                            Rc::clone(&reload_routes_inner),
                            Rc::clone(&on_toast_inner),
                        );
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

/// Import a .zwo, .erg or .mrc file as a custom workout.
fn connect_import(
    import_btn: &gtk::Button,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    workouts_rc: Rc<RefCell<Vec<Workout>>>,
    rebuild: Rc<dyn Fn()>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
) {
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
                if meta.len() > MAX_IMPORT_BYTES {
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

/// Bind Ctrl+F to reveal the search bar and put the cursor in it.
///
/// `GtkSearchBar`'s key capture only handles printable keys and Escape, so the
/// accelerator the toolbar's tooltip advertises has to be installed by hand.
/// The scope is local: the shortcut belongs to this page, and should not fire
/// while the rider is looking at a different one.
fn connect_search_shortcut(
    root: &gtk::Box,
    search_bar: &gtk::SearchBar,
    search_entry: &gtk::SearchEntry,
) {
    let Some(trigger) = gtk::ShortcutTrigger::parse_string("<Control>f") else {
        tracing::error!("Could not parse the Ctrl+F accelerator");
        return;
    };

    let search_bar = search_bar.clone();
    let search_entry = search_entry.clone();
    let action = gtk::CallbackAction::new(move |widget, _| {
        // Global scope means this fires wherever focus is in the window, so the
        // page has to decide for itself whether the shortcut is its to take.
        // Only the ViewStack's visible child is mapped, which is exactly the
        // question being asked.
        if !widget.is_mapped() {
            return glib::Propagation::Proceed;
        }
        search_bar.set_search_mode(true);
        search_entry.grab_focus();
        glib::Propagation::Stop
    });

    // Not `Local`: the rider reaches this page by clicking a sidebar row, so
    // focus stays in the sidebar and a page-local shortcut would never fire.
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);
    controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    root.add_controller(controller);
}
