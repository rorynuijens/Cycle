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

pub(crate) const OUT_DIR: &str = "/tmp/cycle-shots";

/// Start GTK and Adwaita, and make somewhere to write to.
pub(crate) fn start() {
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
pub(crate) fn theme(dark: bool) {
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
pub(crate) fn shoot(window: &impl IsA<gtk::Window>, width: i32, height: i32, name: &str) {
    // Generic over the window type: most shots are a bare `AdwWindow`, but the
    // ride overlay is a real `AdwApplicationWindow` and is shot as itself.
    let window: &gtk::Window = window.as_ref();
    window.set_default_size(width, height);
    window.present();

    // Drain the main loop so realize, allocate and CSS have all happened, and
    // keep trying if the first pass came back empty. One drain was enough for
    // the opaque windows here, but the ride overlay paints on a transparent
    // ground, and on a cold first run after a rebuild it produced no node at
    // all — a spurious failure in a tool whose whole job is to be looked at.
    let node = (0..10)
        .find_map(|_| {
            for _ in 0..200 {
                while glib::MainContext::default().iteration(false) {}
            }
            let paintable = gtk::WidgetPaintable::new(Some(window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, width as f64, height as f64);
            snapshot.to_node()
        })
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

/// The cockpit riding the ramp test, part-way up the ladder.
///
/// The two things test mode changes are both here: "Remaining" has become
/// "Step (of 20)" with the step number under it, and "End Workout" has become
/// "I'm done" and stopped being red.
fn shoot_ramp_player(name: &str, width: i32, height: i32) {
    use crate::data::session::LiveReadings;

    let athlete = crate::data::athlete::AthleteProfile {
        ftp_watts: 200,
        ..crate::data::athlete::AthleteProfile::default()
    };
    let test = Workout::ramp_test();
    let page = crate::ui::pages::player::PlayerPage::new(&test, &athlete);
    // `new` reads the workout, but the caption and button are switched over by
    // `reset_workout`, which is the path a real ride takes.
    page.reset_workout(&test, athlete.ftp_watts);
    page.add_connected_device("AA:BB", "Elite Drivo");

    // Step 9 of the ladder: 10 min warm-up plus 8 whole minutes, 24 s into the
    // ninth. Its target is 124 % of 200 W, and the rider is just holding it.
    let readings = LiveReadings {
        power_watts: Some(246),
        heart_rate_bpm: Some(171),
        cadence_rpm: Some(94),
        speed_kmh: Some(34.8),
        resistance_target_watts: Some(248),
        ..Default::default()
    };
    page.set_readings(readings.clone());
    page.update_from_snapshot(&crate::training::engine::EngineSnapshot {
        state: crate::training::engine::EngineState::Running,
        elapsed_secs: 10 * 60 + 8 * 60 + 24,
        remaining_secs: 12 * 60 + 36,
        segment_index: 9,
        segment_elapsed_secs: 24,
        segment_remaining_secs: 36,
        target_power_watts: 248,
        intensity_pct: 100,
        readings,
    });

    let window = adw::Window::builder().content(page.widget()).build();
    shoot(&window, width, height, name);
}

/// The summary after a ramp test, with the result offered.
fn shoot_ramp_summary(name: &str, height: i32) {
    let test = Workout::ramp_test();
    let session = ridden_ramp_test();

    let athlete = crate::data::athlete::AthleteProfile {
        ftp_watts: 200,
        ..crate::data::athlete::AthleteProfile::default()
    };
    let page = crate::ui::pages::summary::SummaryPage::new(|| {});
    page.update(&session, "Ramp Test", &athlete, Some(&test.segments));
    page.show_ftp_test_result(&session, Some(&test.segments), Rc::new(|_| {}));

    let window = adw::Window::builder().content(page.widget()).build();
    shoot(&window, 900, height, name);
}

/// A ramp test ridden to step nine and then abandoned, as one is.
///
/// ERG holds the target, so recorded power tracks it until the rider runs out of
/// legs half-way up the ninth step.
fn ridden_ramp_test() -> crate::data::session::Session {
    use crate::data::session::DataPoint;

    let test = Workout::ramp_test();
    let mut session = crate::data::session::Session::new(Some(test.id));
    session.ftp_watts = Some(200);
    session.is_ftp_test = true;
    session.rpe = Some(10);

    // Ten minutes of warm-up, then eight whole steps, then thirty seconds of
    // the ninth before it comes apart.
    let ridden = 10 * 60 + 8 * 60 + 30;
    for sec in 0..ridden {
        let target = test
            .segments
            .iter()
            .scan(0u32, |start, seg| {
                let this = *start;
                *start += seg.duration_secs;
                Some((this, seg))
            })
            .find(|(start, seg)| sec >= *start && sec < start + seg.duration_secs)
            .map(|(start, seg)| seg.target_power_at(sec - start, 200))
            .unwrap_or(0);
        // Holding target until the last twenty seconds, then falling away.
        let power = if sec > ridden - 20 {
            target.saturating_sub(70)
        } else {
            target
        };
        session.data_points.push(DataPoint {
            elapsed_secs: sec,
            power_watts: Some(power),
            target_watts: Some(target),
            heart_rate_bpm: Some(120 + (sec / 40).min(60)),
            cadence_rpm: Some(92),
            speed_kmh: Some(32.0),
            lat: None,
            lng: None,
            altitude_m: None,
        });
    }
    session.ended_at = Some(session.started_at + chrono::Duration::seconds(ridden as i64));
    session
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

/// A ride whose trainer went quiet 27 minutes in: 61 minutes on the clock, 34
/// of them with no power. The case the integrity check exists for, and the one
/// the notice has to explain.
fn ride_that_lost_its_trainer() -> crate::data::session::Session {
    use chrono::TimeZone;

    let started = chrono::Utc
        .with_ymd_and_hms(2026, 9, 12, 9, 40, 0)
        .single()
        .expect("hardcoded valid instant");
    let mut ride = crate::data::session::Session::new(Some(9));
    ride.id = 12;
    ride.started_at = started;
    ride.ended_at = Some(started + chrono::Duration::minutes(61));
    ride.ftp_watts = Some(200);
    ride.rpe = Some(6);
    ride.data_points = (0..3660)
        .map(|secs| crate::data::session::DataPoint {
            elapsed_secs: secs,
            // Power for the first 27 minutes, then the trainer stops reporting
            // while the recorder keeps writing — which is what a dropout looks
            // like from inside the file.
            power_watts: (secs < 1620).then(|| 170 + (secs / 60) % 40),
            heart_rate_bpm: Some(138 + (secs / 120) % 14),
            cadence_rpm: Some(88),
            speed_kmh: Some(31.0),
            target_watts: Some(185),
            lat: None,
            lng: None,
            altitude_m: None,
        })
        .collect();
    ride
}

/// The real ride detail view, for a ride that did not record properly.
///
/// `show_session_detail` builds and presents its own `AdwWindow` rather than
/// returning one, so the shot finds it among the toplevels afterwards. Unlike an
/// `AdwDialog` (see the note on `shoot_detail_dialog`) an ordinary window
/// allocates and renders offscreen without any help.
fn shoot_session_detail(session: &crate::data::session::Session, name: &str, height: i32) {
    use chrono::TimeZone;

    let rt = tokio::runtime::Runtime::new().expect("a runtime for the dialog's writes");
    let pool = rt
        .block_on(async {
            let pool = sqlx::SqlitePool::connect(":memory:").await?;
            crate::data::migrate::run(&pool).await?;
            Ok::<_, anyhow::Error>(pool)
        })
        .expect("an empty database");

    // Presented against a host: the detail window is modal, and a modal window
    // with nothing to be modal for never maps, so it is never allocated and
    // renders as nothing at all.
    let host = adw::Window::builder().build();
    host.set_content(Some(&adw::ToolbarView::new()));
    host.present();
    while glib::MainContext::default().iteration(false) {}

    let before = toplevels();
    super::detail::show_session_detail(
        session,
        "Sweet Spot Base I",
        chrono::Local
            .with_ymd_and_hms(2026, 9, 12, 9, 40, 0)
            .single()
            .expect("hardcoded valid instant"),
        200,
        72.0,
        None,
        Some(host.upcast_ref()),
        pool,
        rt.handle().clone(),
        Rc::new(RefCell::new(None)),
    );
    while glib::MainContext::default().iteration(false) {}

    let presented = toplevels()
        .into_iter()
        .find(|w| !before.iter().any(|b| b == w))
        .and_then(|w| w.downcast::<adw::Window>().ok())
        .expect("the detail window was presented");

    // The content is lifted onto a window of our own rather than rendered where
    // it was presented, the same way `shoot_detail_dialog` handles a dialog:
    // the real one is modal for the host, and a modal window is drawn as part of
    // its host's stack rather than on its own.
    let content = presented.content().expect("the detail window has content");
    presented.set_content(None::<&gtk::Widget>);
    presented.close();
    host.close();
    while glib::MainContext::default().iteration(false) {}

    let window = adw::Window::builder().content(&content).build();
    shoot(&window, 440, height, name);
}

/// Every window currently open, so a newly presented one can be picked out.
fn toplevels() -> Vec<gtk::Window> {
    let list = gtk::Window::toplevels();
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_then(|o| o.downcast::<gtk::Window>().ok()))
        .collect()
}

/// The summary the rider sees when they get off the bike, for the same ride.
fn shoot_summary(session: &crate::data::session::Session, name: &str, height: i32) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime for the summary's reads");
    let pool = rt
        .block_on(async {
            let pool = sqlx::SqlitePool::connect(":memory:").await?;
            crate::data::migrate::run(&pool).await?;
            Ok::<_, anyhow::Error>(pool)
        })
        .expect("an empty database");

    let athlete = crate::data::athlete::AthleteProfile {
        ftp_watts: 200,
        ..crate::data::athlete::AthleteProfile::default()
    };
    let page = crate::ui::pages::summary::SummaryPage::new(|| {});
    page.update(session, "Sweet Spot Base I", &athlete, None);
    page.show_integrity(
        session,
        pool,
        rt.handle(),
        std::sync::Arc::new(std::sync::Mutex::new(Some(12))),
    );

    let window = adw::Window::builder().content(page.widget()).build();
    shoot(&window, 900, height, name);
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

// ── The ride overlay ─────────────────────────────────────────────────────────

/// What the overlay needs besides widgets: an application to belong to, and a
/// database and runtime it only ever uses to remember its size and opacity.
///
/// The application is never registered or run — nothing here reaches D-Bus. It
/// exists because `AdwApplicationWindow` must belong to one, and the overlay is
/// an application window on purpose (CLAUDE.md §1.1).
fn overlay_context() -> (adw::Application, sqlx::SqlitePool, tokio::runtime::Runtime) {
    let app = adw::Application::builder()
        .application_id("io.github.rorynuijens.Cycle.Shots")
        .build();
    let rt = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let pool = rt
        .block_on(sqlx::SqlitePool::connect(":memory:"))
        .expect("an in-memory database");
    (app, pool, rt)
}

/// The overlay, showing one tick of a ride already under way.
fn ride_overlay(
    app: &adw::Application,
    pool: &sqlx::SqlitePool,
    rt: &tokio::runtime::Runtime,
    power: u32,
) -> crate::ui::overlay::RideOverlay {
    use crate::data::session::LiveReadings;
    use crate::training::engine::{EngineSnapshot, EngineState};

    let w = workout(9, "Aerobic Foundation", WorkoutCategory::Endurance, 46.0);
    let overlay = crate::ui::overlay::RideOverlay::new(
        app.upcast_ref::<gtk::Application>(),
        &w,
        200,
        crate::data::settings::OverlaySettings::default(),
        pool.clone(),
        rt.handle().clone(),
    );
    overlay.update(
        &EngineSnapshot {
            state: EngineState::Running,
            elapsed_secs: 742,
            remaining_secs: 1418,
            segment_index: 2,
            segment_elapsed_secs: 96,
            segment_remaining_secs: 84,
            target_power_watts: 205,
            intensity_pct: 100,
            readings: LiveReadings {
                power_watts: Some(power),
                heart_rate_bpm: Some(142),
                cadence_rpm: Some(88),
                ..LiveReadings::default()
            },
        },
        &[],
    );
    overlay
}

/// The overlay must not move when a number gains a digit.
///
/// The same rule the cockpit is held to, and it bites harder here: this window
/// is a few hundred pixels wide and sits over a film, so a window that resizes
/// itself every time the rider pushes over 100 W is unusable rather than merely
/// untidy.
fn assert_the_overlay_holds_still() {
    let (app, pool, rt) = overlay_context();
    let widths: Vec<i32> = [9u32, 99, 187, 999, 1240]
        .into_iter()
        .map(|power| {
            let overlay = ride_overlay(&app, &pool, &rt, power);
            overlay.window().set_default_size(420, 312);
            overlay.present();
            while glib::MainContext::default().iteration(false) {}
            let min = overlay.window().measure(gtk::Orientation::Horizontal, -1).0;
            overlay.close();
            min
        })
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "the overlay's minimum width changes with the power reading: {widths:?}"
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
    // The ride cockpit. Rendered at three sizes: a big window, an ordinary one,
    // and a small one — the last is the check that a taller number block plus a
    // graph floor still fits without a scrollbar (the cockpit sizing rule).
    shoot_player("10-player-1400x900", 1400, 900);
    shoot_player("11-player-1100x780", 1100, 780);
    shoot_player("12-player-900x700", 900, 700);
    shoot_player_at("12b-player-sprint-900", 900, 700, 1240);
    shoot_player_cue("13-player-cue", 1100, 780);

    // Ahead of the route and week shots below, deliberately: `shoot_route_player`
    // builds a Shumate map, and with no tile server reachable it either fails to
    // produce a render node or sits waiting on the network for as long as it is
    // given. Anything sequenced after it may simply never run.
    // A ride that did not record properly, in both places it is said: the
    // summary the rider lands on, and the detail view they open later. Shot in
    // both themes because the notice is the one place in the app that leans on
    // the warning colour (CLAUDE.md §4.2).
    {
        let broken = ride_that_lost_its_trainer();
        shoot_summary(&broken, "20-summary-flagged-light", 900);
        shoot_session_detail(&broken, "21-detail-flagged-light", 720);
        let mut counted = broken.clone();
        counted.integrity_dismissed = true;
        shoot_session_detail(&counted, "22-detail-counted-light", 720);
        theme(true);
        shoot_summary(&broken, "23-summary-flagged-dark", 900);
        shoot_session_detail(&broken, "24-detail-flagged-dark", 720);
        theme(false);
    }

    // Checked here rather than in its own test: GTK may only be initialised
    // once per process, and a second `#[test]` is a second thread.
    assert_the_cockpit_holds_still();
    assert_the_route_cockpit_holds_still();
    assert_the_overlay_holds_still();

    shoot_route_player("14-route-1100x780", 1100, 780);
    shoot_week("15-week-ridden-plan", 1000, 700);

    // The ramp test: the cockpit in test mode, and the result it offers after.
    // Both themes for the summary — the card is the only place in the app that
    // leans on the accent colour for a number (CLAUDE.md §4.2).
    shoot_ramp_player("25-ramp-cockpit-light", 1100, 800);
    shoot_ramp_summary("26-ramp-summary-light", 1000);
    theme(true);
    shoot_ramp_player("27-ramp-cockpit-dark", 1100, 800);
    shoot_ramp_summary("28-ramp-summary-dark", 1000);
    theme(false);

    // The ride overlay, in both themes. What a PNG cannot show is the point of
    // it — the panel is translucent, and whether the numbers stay readable over
    // moving video can only be judged over actual moving video.
    {
        let (app, pool, rt) = overlay_context();
        // Re-assert the theme: building an `AdwApplication` resets the default
        // style manager's colour scheme, so the light shot came out dark.
        theme(false);
        shoot(
            ride_overlay(&app, &pool, &rt, 187).window(),
            420,
            312,
            "18-overlay-light",
        );
        shoot(
            ride_overlay(&app, &pool, &rt, 1240).window(),
            420,
            312,
            "18b-overlay-sprint",
        );
        theme(true);
        shoot(
            ride_overlay(&app, &pool, &rt, 187).window(),
            420,
            312,
            "19-overlay-dark",
        );
        theme(false);
    }

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
