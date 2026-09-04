//! Offscreen renders of real widgets, for reviewing UI without a screen.
//!
//! GNOME denies every screenshot route on this machine — the desktop portal and
//! `org.gnome.Shell.Screenshot` both answer "Screenshot is not allowed" — so a
//! review from away from the machine had nothing to look at. This renders the
//! widgets themselves through GSK, which needs no compositor and no permission:
//! the window is built, presented, allocated, and drawn straight into a PNG.
//!
//! These are a tool, not assertions. They are `#[ignore]`d so a normal
//! `cargo test` never draws anything, and run deliberately:
//!
//! ```text
//! cargo test -- --ignored --test-threads=1 screenshots
//! ```
//!
//! `--test-threads=1` is required, not tidiness: GTK may only be touched from
//! the thread that initialised it (CLAUDE.md §2.3), and two of these running at
//! once are two threads in one GTK.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use super::marks::{EntryMark, Suggestion};
use crate::data::db::{CalendarEntry, ScheduledItem};
use crate::data::workout::{Segment, Workout, WorkoutCategory};

const OUT_DIR: &str = "/tmp/cycle-shots";

/// Start GTK and Adwaita, and make somewhere to write to.
fn start() {
    adw::init().expect("Adwaita starts");
    // Without this an AdwDialog stays at zero size: it opens on an animation,
    // and an animation needs a frame clock that a headless render never ticks.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_enable_animations(false);
    }
    crate::ui::load_css();
    std::fs::create_dir_all(OUT_DIR).expect("output directory");
}

/// Force a theme, so both are shot from one process (CLAUDE.md §4.2).
fn theme(dark: bool) {
    adw::StyleManager::default().set_color_scheme(if dark {
        adw::ColorScheme::ForceDark
    } else {
        adw::ColorScheme::ForceLight
    });
}

/// Draw a presented window into a PNG.
///
/// The window really is presented: a widget that was never allocated has no
/// size and renders as nothing at all, and allocation is the compositor's
/// answer rather than something that can be asserted into place.
fn shoot(window: &adw::Window, width: i32, height: i32, name: &str) {
    window.set_default_size(width, height);
    window.present();
    // Drain the main loop so realize, allocate and CSS have all happened.
    for _ in 0..200 {
        while glib::MainContext::default().iteration(false) {}
    }

    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let node = snapshot
        .to_node()
        .expect("the window produced no render node");

    let renderer = gtk::gsk::CairoRenderer::new();
    renderer
        .realize(None)
        .expect("a Cairo renderer realizes with no surface");
    let texture = renderer.render_texture(&node, None);
    let path = format!("{OUT_DIR}/{name}.png");
    texture.save_to_png(&path).expect("PNG written");
    renderer.unrealize();
    window.close();
    println!("wrote {path}");
}

// ── The fixtures the shots are dressed from ──────────────────────────────────

fn workout(id: i64, name: &str, category: WorkoutCategory, tss: f32) -> Workout {
    Workout {
        id,
        name: name.into(),
        description: String::new(),
        duration_secs: 3600,
        tss,
        category,
        segments: vec![
            Segment::steady(600, 55.0, "Warm-up"),
            Segment::steady(420, 88.0, "Block 1"),
            Segment::steady(240, 55.0, "Recovery"),
            Segment::steady(420, 92.0, "Block 2"),
            Segment::steady(240, 55.0, "Recovery"),
            Segment::steady(420, 95.0, "Block 3"),
            Segment::steady(600, 45.0, "Cool-down"),
        ],
    }
}

fn library() -> Rc<Vec<Workout>> {
    Rc::new(vec![
        workout(1, "Active Recovery 45", WorkoutCategory::Recovery, 15.0),
        workout(2, "Recovery Ride", WorkoutCategory::Recovery, 23.5),
        workout(3, "Endurance 60", WorkoutCategory::Endurance, 48.0),
    ])
}

fn entry(workout_id: i64, name: &str, completed: bool) -> CalendarEntry {
    CalendarEntry {
        id: 47,
        item: ScheduledItem::Workout {
            id: workout_id,
            name: name.into(),
        },
        scheduled_date: "2026-09-11".into(),
        completed,
        category: WorkoutCategory::Recovery,
        tss: 15.0,
        duration_secs: 2700,
        program_id: Some(7),
        adjusted_from: None,
        previous_step_name: None,
    }
}

/// Present the real detail dialog on a real window and shoot it.
#[allow(clippy::too_many_arguments)]
fn shoot_detail_dialog(
    entry: &CalendarEntry,
    mark: EntryMark,
    rides: Vec<crate::training::matching::DayRide>,
    name: &str,
    height: i32,
) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime for the dialog's writes");
    let pool = rt
        .block_on(async {
            let pool = sqlx::SqlitePool::connect(":memory:").await?;
            crate::data::migrate::run(&pool).await?;
            Ok::<_, anyhow::Error>(pool)
        })
        .expect("an empty database");

    // The dialog is presented on a throwaway host, then its content is lifted
    // out and rendered on its own. An AdwDialog opens as a floating sheet, and
    // the sheet allocates itself from an animation — with no compositor there
    // is no frame clock to drive one, so it stays at zero size and renders
    // blank however long the loop is spun. The content inside it is an ordinary
    // widget and lays out normally once it is somewhere ordinary.
    let host = adw::Window::builder().build();
    host.set_content(Some(&adw::ToolbarView::new()));
    host.present();
    while glib::MainContext::default().iteration(false) {}

    let start_route: crate::ui::StartRouteHolder = Rc::new(RefCell::new(None));
    super::dialogs::show_workout_detail_dialog(
        &host,
        entry,
        pool,
        rt.handle().clone(),
        library(),
        Rc::new(|_| {}),
        start_route,
        Rc::new(|| {}),
        Rc::new(|_| {}),
        mark,
        rides,
        211,
    );
    while glib::MainContext::default().iteration(false) {}

    let dialog = find_dialog(host.upcast_ref::<gtk::Widget>()).expect("the dialog was presented");
    let content = dialog.child().expect("the dialog has content");
    dialog.set_child(None::<&gtk::Widget>);
    host.close();

    let window = adw::Window::builder().build();
    window.set_content(Some(&content));
    shoot(&window, 480, height, name);
}

/// The first AdwDialog anywhere under `w`.
fn find_dialog(w: &gtk::Widget) -> Option<adw::Dialog> {
    if let Some(d) = w.downcast_ref::<adw::Dialog>() {
        return Some(d.clone());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(found) = find_dialog(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

fn eased_twice_mark() -> EntryMark {
    EntryMark {
        program_week: Some((2, 12)),
        adjusted_from: Some("Endurance 60".into()),
        previous_step_name: Some("Recovery Ride".into()),
        suggestion: None,
    }
}

// ── The shots ────────────────────────────────────────────────────────────────

/// Render every shot, in one test on one thread.
///
/// One test, not six: `--test-threads=1` still gives each test its *own*
/// thread, and GTK refuses to be initialised from a second one. So the whole
/// set runs in sequence here, which also lets the theme be switched partway
/// through rather than started twice.
#[test]
#[ignore = "a tool for reviewing UI, not an assertion"]
fn screenshots() {
    start();
    theme(false);

    // Two eases deep: the day names its origin, the button names one rung back.
    shoot_detail_dialog(
        &entry(1, "Active Recovery 45", false),
        eased_twice_mark(),
        vec![],
        "01-eased-twice-light",
        720,
    );

    // What the redraw leaves behind after one press, without reopening: the
    // middle workout is on the day, and the button now offers the origin.
    shoot_detail_dialog(
        &entry(2, "Recovery Ride", false),
        EntryMark {
            previous_step_name: Some("Endurance 60".into()),
            ..eased_twice_mark()
        },
        vec![],
        "02-after-one-undo",
        720,
    );

    // Eased already, and the rules still want it easier: both rows at once.
    shoot_detail_dialog(
        &entry(1, "Active Recovery 45", false),
        EntryMark {
            suggestion: Some(Suggestion {
                to_workout_id: 2,
                to_name: "Recovery Ride".into(),
                reason: "Your form is -18 and last night's sleep was poor.".into(),
            }),
            ..eased_twice_mark()
        },
        vec![],
        "03-eased-and-suggested",
        800,
    );

    // The "N intervals" caption used to be gated on `!completed` and vanished
    // from a day the moment it was marked done. The graph is new here too.
    shoot_detail_dialog(
        &entry(3, "Endurance 60", true),
        EntryMark {
            program_week: Some((2, 12)),
            ..EntryMark::default()
        },
        vec![crate::training::matching::DayRide {
            name: "Maltepe Road Cycling".into(),
            duration_secs: 5340,
            tss: Some(106.0),
        }],
        "04-completed-keeps-graph",
        760,
    );

    // A route has no power profile to draw, and must not get an empty one.
    let mut route_day = entry(1, "Alpe d'Huez", false);
    route_day.item = ScheduledItem::Route {
        id: 4,
        name: "Alpe d'Huez".into(),
    };
    route_day.category = WorkoutCategory::Endurance;
    shoot_detail_dialog(
        &route_day,
        EntryMark::default(),
        vec![],
        "06-route-no-graph",
        560,
    );

    theme(true);
    shoot_detail_dialog(
        &entry(1, "Active Recovery 45", false),
        eased_twice_mark(),
        vec![],
        "05-eased-twice-dark",
        720,
    );
}
