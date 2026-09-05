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

use super::marks::{EntryMark, ProgramOverlay, Suggestion};
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

/// A cockpit mid-interval, at whatever power the shot wants to show.
fn player_page(power: u32) -> crate::ui::pages::player::PlayerPage {
    use crate::data::session::LiveReadings;

    let athlete = crate::data::athlete::AthleteProfile {
        ftp_watts: 200,
        ..crate::data::athlete::AthleteProfile::default()
    };
    let w = workout(9, "Aerobic Foundation", WorkoutCategory::Endurance, 46.0);
    let page = crate::ui::pages::player::PlayerPage::new(&w, &athlete);

    page.add_connected_device("AA:BB", "Elite Drivo");
    page.add_connected_device("CC:DD", "Wahoo TICKR");
    // The labels are written by the tick, not by `set_readings` — that only
    // stores. Drive one snapshot through so the cockpit shows real numbers,
    // which is the whole point of looking at it.
    let readings = LiveReadings {
        power_watts: Some(power),
        heart_rate_bpm: Some(142),
        cadence_rpm: Some(88),
        speed_kmh: Some(31.4),
        resistance_target_watts: Some(180),
        ..Default::default()
    };
    page.set_readings(readings.clone());
    page.update_from_snapshot(&crate::training::engine::EngineSnapshot {
        state: crate::training::engine::EngineState::Running,
        elapsed_secs: 1284,
        remaining_secs: 2316,
        segment_index: 3,
        segment_elapsed_secs: 96,
        segment_remaining_secs: 324,
        target_power_watts: 180,
        intensity_pct: 100,
        readings,
    });
    page
}

/// The ride cockpit, dressed with live numbers.
///
/// Sited here rather than beside `player.rs` because GTK may only be
/// initialised once per process: every shot has to run inside the one `#[test]`
/// below, whatever page it is of.
fn shoot_player(name: &str, width: i32, height: i32) {
    shoot_player_at(name, width, height, 187)
}

/// The same, at a chosen power — the four-digit case has to be looked at, not
/// only measured.
fn shoot_player_at(name: &str, width: i32, height: i32, power: u32) {
    let page = player_page(power);
    let window = adw::Window::builder().content(page.widget()).build();
    shoot(&window, width, height, name);
}

/// The cockpit with its interval cue showing, which is the part made of words.
fn shoot_player_cue(name: &str, width: i32, height: i32) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime for the history read");
    let pool = rt
        .block_on(async {
            let pool = sqlx::SqlitePool::connect(":memory:").await?;
            crate::data::migrate::run(&pool).await?;
            Ok::<_, anyhow::Error>(pool)
        })
        .expect("an empty database");

    let w = workout(9, "Aerobic Foundation", WorkoutCategory::Endurance, 46.0);
    let page = Rc::new(RefCell::new(player_page(187)));
    // The real path: cues are built off the runtime and land on the main
    // thread. An empty database simply means no history line.
    crate::ui::pages::player::load_cues(Rc::clone(&page), w, pool, rt.handle());
    // The read is a round trip through the runtime: drain the loop until the
    // cues land rather than once, which is a race the empty database wins.
    for _ in 0..200 {
        while glib::MainContext::default().iteration(false) {}
        if !page.borrow().has_cues() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let p = page.borrow();
    p.update_from_snapshot(&crate::training::engine::EngineSnapshot {
        state: crate::training::engine::EngineState::Running,
        elapsed_secs: 1284,
        remaining_secs: 2316,
        segment_index: 3,
        segment_elapsed_secs: 96,
        segment_remaining_secs: 324,
        target_power_watts: 180,
        intensity_pct: 90,
        readings: crate::data::session::LiveReadings {
            power_watts: Some(187),
            heart_rate_bpm: Some(142),
            cadence_rpm: Some(88),
            ..Default::default()
        },
    });

    let window = adw::Window::builder().content(p.widget()).build();
    shoot(&window, width, height, name);
}

/// One week of the calendar, as the rider reads it.
///
/// Dressed with the day that started this: a program session ridden in the app,
/// which writes both a completed plan and a recorded ride. They are one session
/// and the list shows them as one row
/// ([`crate::training::matching::plans_closed_by_a_ride`]).
fn shoot_week(name: &str, width: i32, height: i32) {
    use chrono::TimeZone;

    let rt = tokio::runtime::Runtime::new().expect("a runtime for the row's writes");
    let pool = rt
        .block_on(async {
            let pool = sqlx::SqlitePool::connect(":memory:").await?;
            crate::data::migrate::run(&pool).await?;
            Ok::<_, anyhow::Error>(pool)
        })
        .expect("an empty database");

    let monday = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).expect("hardcoded valid date");
    let saturday = monday + chrono::Duration::days(5);

    let mut plan = entry(9, "Aerobic Foundation", true);
    plan.scheduled_date = saturday.to_string();
    plan.category = WorkoutCategory::Endurance;
    plan.tss = 46.0;
    plan.duration_secs = 4500;
    plan.program_id = Some(1);

    let started = chrono::Utc
        .with_ymd_and_hms(2026, 9, 5, 9, 37, 0)
        .single()
        .expect("hardcoded valid instant");
    let mut ride = crate::data::session::Session::new(Some(9));
    ride.id = 8;
    ride.started_at = started;
    ride.ended_at = Some(started + chrono::Duration::minutes(75));
    ride.ftp_watts = Some(200);
    // A flat 190 W for 75 minutes: enough data points to score, few enough to
    // build in a screenshot.
    ride.data_points = (0..4500)
        .step_by(5)
        .map(|secs| crate::data::session::DataPoint {
            elapsed_secs: secs,
            power_watts: Some(190),
            heart_rate_bpm: Some(142),
            cadence_rpm: Some(88),
            speed_kmh: Some(31.0),
            target_watts: Some(185),
            lat: None,
            lng: None,
            altitude_m: None,
        })
        .collect();

    let events = vec![
        crate::data::calendar::CalendarEvent::Scheduled(plan),
        crate::data::calendar::CalendarEvent::Session(
            crate::data::db::SessionRecord {
                session: ride,
                workout_name: Some("Aerobic Foundation".into()),
                uploaded_to_icu: false,
            },
            Some("Aerobic Foundation".into()),
        ),
    ];

    let overlay = Rc::new(ProgramOverlay {
        program: Some(crate::training::program::Program {
            id: 1,
            start_monday: monday - chrono::Duration::weeks(11),
            num_weeks: 15,
            training_days: "tuesday,thursday,saturday".into(),
        }),
        adjustment: None,
    });

    let start_route: crate::ui::StartRouteHolder = Rc::new(RefCell::new(None));
    let week = super::week::build_week_view(
        monday,
        &events,
        pool,
        rt.handle().clone(),
        Rc::new(RefCell::new(None)),
        library(),
        Rc::new(|_| {}),
        start_route,
        200,
        72.0,
        Rc::new(|_| {}),
        overlay,
    );

    let window = adw::Window::builder().build();
    let clamp = adw::Clamp::builder()
        .maximum_size(1000)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .child(&week)
        .build();
    window.set_content(Some(&clamp));
    shoot(&window, width, height, name);
}

/// A route the shots can ride: a straight 12 km at 8 %.
fn climb() -> crate::data::route::Route {
    crate::data::route::Route {
        name: "Alpe d'Huez".into(),
        points: (0..400)
            .map(|i| {
                let d = i as f32 * 30.0;
                crate::data::route::RoutePoint {
                    lat: 45.05 + i as f64 * 0.0002,
                    lng: 6.05 + i as f64 * 0.0001,
                    elevation_m: 720.0 + d * 0.08,
                    distance_m: d,
                    gradient: 0.08,
                }
            })
            .collect(),
        total_distance_m: 12_000.0,
        total_gain_m: 960.0,
    }
}

/// The route cockpit: same metric columns, a road instead of a plan.
fn shoot_route_player(name: &str, width: i32, height: i32) {
    let page = crate::ui::pages::route_player::RoutePlayerPage::new(&climb(), 200);
    let window = adw::Window::builder().content(page.widget()).build();
    // Its numbers are written by the ride loop, which needs a trainer. Dress
    // them directly so the shot shows a cockpit rather than a row of dashes.
    write_numbers(
        page.widget().upcast_ref::<gtk::Widget>(),
        &["-8.4", "1240", "62.7", "168", "94", "1:42:07", "1240"],
    );
    shoot(&window, width, height, name);
}

/// Put `values` into the page's numeric labels, in tree order.
///
/// Only for the offscreen renders: the route page's readouts are written from
/// inside its ride loop, which cannot run without a trainer on the other end.
/// What this dresses is the layout, never the formatting — that is the ride
/// loop's own and is not exercised here.
fn write_numbers(w: &gtk::Widget, values: &[&str]) {
    fn walk(w: &gtk::Widget, values: &[&str], next: &mut usize) {
        if let Some(label) = w.downcast_ref::<gtk::Label>() {
            if w.has_css_class("numeric") && *next < values.len() {
                label.set_label(values[*next]);
                *next += 1;
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            walk(&c, values, next);
            child = c.next_sibling();
        }
    }
    walk(w, values, &mut 0);
}

/// The cockpit must not move when a number gains a digit.
///
/// Asserted rather than eyeballed, and from the window's *minimum* width, which
/// is what a value too wide for its column actually costs: the hero row is
/// `column_homogeneous`, so one more digit on the power number used to widen
/// every column in the row. Measured before the fix, this read 777 px at two
/// digits, 951 px at three and 1128 px at four — the window growing under the
/// rider mid-sprint, and past the 900 px clamp at four.
///
/// Lives in this module because it needs a display, like everything else here.
fn assert_the_cockpit_holds_still() {
    let widths: Vec<i32> = [9u32, 99, 187, 999, 1240]
        .into_iter()
        .map(|power| {
            let page = player_page(power);
            let window = adw::Window::builder().content(page.widget()).build();
            window.set_default_size(900, 800);
            window.present();
            while glib::MainContext::default().iteration(false) {}
            let min = window.measure(gtk::Orientation::Horizontal, -1).0;
            window.close();
            min
        })
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "the cockpit's minimum width changes with the power reading: {widths:?}"
    );
    assert!(
        widths[0] <= crate::ui::WINDOWED_CLAMP,
        "the cockpit no longer fits its own clamp: {} px",
        widths[0]
    );
}

/// The same, for the route cockpit, whose hero row is built the same way.
fn assert_the_route_cockpit_holds_still() {
    let short = ["0.0", "9", "8.1", "99", "70", "0:00", "12"];
    let long = ["-12.5", "1240", "72.4", "168", "112", "1:42:07", "1240"];
    let widths: Vec<i32> = [short, long]
        .into_iter()
        .map(|values| {
            let page = crate::ui::pages::route_player::RoutePlayerPage::new(&climb(), 200);
            let window = adw::Window::builder().content(page.widget()).build();
            window.set_default_size(900, 800);
            window.present();
            write_numbers(page.widget().upcast_ref::<gtk::Widget>(), &values);
            while glib::MainContext::default().iteration(false) {}
            let min = window.measure(gtk::Orientation::Horizontal, -1).0;
            window.close();
            min
        })
        .collect();
    assert_eq!(
        widths[0], widths[1],
        "the route cockpit's minimum width changes with its readings: {widths:?}"
    );
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

    // Checked here rather than in its own test: GTK may only be initialised
    // once per process, and a second `#[test]` is a second thread.
    assert_the_cockpit_holds_still();
    assert_the_route_cockpit_holds_still();

    // The ride cockpit. Rendered at three sizes: a big window, an ordinary one,
    // and a small one — the last is the check that a taller number block plus a
    // graph floor still fits without a scrollbar (the cockpit sizing rule).
    shoot_player("10-player-1400x900", 1400, 900);
    shoot_player("11-player-1100x780", 1100, 780);
    shoot_player("12-player-900x700", 900, 700);
    shoot_player_at("12b-player-sprint-900", 900, 700, 1240);
    shoot_player_cue("13-player-cue", 1100, 780);
    shoot_route_player("14-route-1100x780", 1100, 780);
    shoot_week("15-week-ridden-plan", 1000, 700);

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
                harder: false,
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
    shoot_player("16-player-dark", 1100, 780);
    shoot_detail_dialog(
        &entry(1, "Active Recovery 45", false),
        eased_twice_mark(),
        vec![],
        "05-eased-twice-dark",
        720,
    );
}
