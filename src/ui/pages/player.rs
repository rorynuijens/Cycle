use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// A transport button's action, filled in once the engine exists.
///
/// The same type the pages use for their reload callbacks, and read through the
/// same [`crate::ui::call_reload`]: these are handlers on widgets the callback
/// itself may rebuild, which is the hazard that type exists to contain
/// (CLAUDE.md §2.4). They were `Box` only because they predate it, and a `Box`
/// cannot be cloned out of the `RefCell` before the call.
type ButtonCb = crate::ui::ReloadHolder;

use crate::data::biome::WorldLook;
use crate::data::settings;
use crate::data::workout_road::WorkoutRoad;
use crate::data::{
    athlete::AthleteProfile,
    session::{LiveReadings, ReadingsTracker, Session},
    workout::Workout,
};
use crate::training::auto_pause::{self, AutoPause, RideState};
use crate::training::cues::{SegmentCue, CLOSING_SECS};
use crate::training::engine::{
    is_pedalling, EngineSnapshot, EngineState, WorkoutEngine, INTENSITY_STEP_PCT, PRE_START_SECS,
};
use crate::ui::overlay::RideOverlay;
use crate::ui::pages::route_immersion::{Immersion, Parts};
use crate::ui::widgets::course_view::CourseView;
use crate::ui::widgets::metric_column::{
    caption_text, metric_column, metric_column_parts, CLOCK_DIGITS, POWER_DIGITS, RATE_DIGITS,
};
use crate::ui::widgets::ride_hud::{self, WorkoutHudReadings};
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::widgets::zone_meter::ZoneMeter;
use crate::ui::WINDOWED_CLAMP;

/// Minimum height for the workout profile strip, in pixels.
///
/// The floor from the cockpit sizing rule: enough that the shape of the session
/// is legible at a glance, small enough that it can never be what the page is
/// mostly made of. Spare height goes to the numbers instead.
const GRAPH_FLOOR_PX: i32 = 180;

/// Minimum height for the 3D road in the graph's slot, in pixels: the route
/// page's map floor.
const ROAD_FLOOR_PX: i32 = 180;

pub struct PlayerPage {
    root: gtk::Box,
    power_label: gtk::Label,
    hr_label: gtk::Label,
    cadence_label: gtk::Label,
    target_label: gtk::Label,
    /// The intensity dial's reading, under the target — shown only off 100 %.
    target_intensity: gtk::Label,
    elapsed_label: gtk::Label,
    remaining_label: gtk::Label,
    /// Caption over [`Self::remaining_label`]. Kept because the ramp test
    /// repurposes that column — see [`PlayerPage::set_test_mode`].
    remaining_caption: gtk::Label,
    interval_label: gtk::Label,
    workout_progress: gtk::ProgressBar,
    segment_progress: gtk::ProgressBar,
    /// Caption above the interval countdown — shows "Interval N" while running.
    interval_caption: gtk::Label,
    /// Live power-zone ribbon under the hero power number.
    zone_meter: ZoneMeter,
    /// The compact window the rider can pin over a film, while it is open.
    /// `None` whenever it is closed, which is the usual case.
    overlay: Rc<RefCell<Option<RideOverlay>>>,
    /// Fires when the overlay button is pressed; installed by `start_timer`,
    /// which is where the ride being overlaid is known.
    overlay_cb: ButtonCb,
    /// Shown during the pre-start countdown — hides once the engine starts.
    countdown_banner: adw::Banner,
    /// Raised mid-ride when a sensor stops reporting. A dropout used to show up
    /// only as a chip quietly leaving the row above, which is far too easy to
    /// miss while riding hard.
    dropout_banner: adw::Banner,
    /// Horizontal pill row listing connected BLE devices.
    devices_section: gtk::Box,
    devices_flow: gtk::Box,
    graph: WorkoutGraph,
    avg_power_total: gtk::Label,
    np_total: gtk::Label,
    if_total: gtk::Label,
    tss_total: gtk::Label,
    kj_total: gtk::Label,
    pause_btn: gtk::Button,
    end_btn: gtk::Button,
    /// Reads the intensity dial's position, e.g. "90%".
    intensity_label: gtk::Label,
    /// Visible only while the engine is in Idle state; lets the user back out
    /// before the workout has actually started.
    cancel_btn: gtk::Button,
    /// Latest readings from the device manager, updated by the GLib event loop.
    pub last_readings: Rc<RefCell<ReadingsTracker>>,
    /// Consecutive seconds of power data received while the engine is Idle.
    pub power_countdown: Rc<Cell<u32>>,
    /// Callback wired by `start_timer` for the countdown banner's "Start Now" button.
    start_now_cb: ButtonCb,
    /// Playback-control callbacks — set (and replaced) each time `start_timer` is called,
    /// so that buttons are wired exactly once in `new()` and never accumulate handlers.
    end_cb: ButtonCb,
    pause_cb: ButtonCb,
    skip_cb: ButtonCb,
    cancel_cb: ButtonCb,
    intensity_down_cb: ButtonCb,
    intensity_up_cb: ButtonCb,
    /// Shows the active workout name above the graph.
    workout_name_label: gtk::Label,
    /// ── Interval cues ───────────────────────────────────────────────────────
    /// What this interval is for, in words: which rep it is, how many remain,
    /// how to ride it. Built once per ride off the main thread (the history it
    /// draws on lives in the database) and then read by segment index.
    cue_box: gtk::Box,
    cue_headline: gtk::Label,
    cue_detail: gtk::Label,
    cues: RefCell<Vec<SegmentCue>>,
    /// Whether the rider wants cues at all — Preferences → Training.
    cues_enabled: Cell<bool>,
    /// What is currently rendered, so the labels and CSS classes are only
    /// touched when the cue actually changes rather than every tick.
    cue_rendered: Cell<Option<(usize, bool)>>,
    /// Bumped by every `reset_workout`. A history read in flight when the rider
    /// backs out and starts something else would otherwise land its rep numbers
    /// on the wrong workout.
    cue_generation: Cell<u64>,
    /// Whether this ride is an FTP test rather than a workout to be completed.
    ///
    /// A ramp test has no end to count down to: it ends when the rider can no
    /// longer hold the step. So the cockpit counts steps instead of time
    /// remaining, and the end button reads as finishing the test rather than
    /// abandoning a session. Set by [`PlayerPage::reset_workout`] from
    /// [`Workout::is_ramp_test`], which is also what stamps the recorded ride.
    test_mode: Cell<bool>,
    /// Ladder steps in the test being ridden, for the "of N" in the caption.
    /// Meaningless unless [`Self::test_mode`] is set.
    ladder_steps: Cell<u32>,
    /// The layout for windowed, fullscreen and fullscreen on the road — see
    /// [`PlayerPage::set_fullscreen`].
    immersion: Rc<Immersion>,
    /// The on-page fullscreen button, which turns into the way back out.
    fullscreen_btn: gtk::Button,
    /// ── The 3D road ─────────────────────────────────────────────────────────
    course_view: CourseView,
    view_3d_btn: gtk::ToggleButton,
    /// The road the workout is being ridden along, if it has one.
    road: RefCell<Option<WorkoutRoad>>,
    /// Set while [`PlayerPage::set_world`] puts the 3D button where the rider
    /// left it, so that is not saved back as a choice.
    restoring_road: Rc<Cell<bool>>,
}

impl PlayerPage {
    pub fn new(workout: &Workout, athlete: &AthleteProfile) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Countdown banner (pre-start) ─────────────────────────────────────
        let countdown_banner = adw::Banner::builder()
            .title("Connect a power meter or trainer to begin")
            .button_label("Start Now")
            .revealed(true)
            .build();

        let start_now_cb: ButtonCb = Rc::new(RefCell::new(None));
        let start_now_ref = Rc::clone(&start_now_cb);
        countdown_banner.connect_button_clicked(move |_| {
            crate::ui::call_reload(&start_now_ref);
        });

        root.append(&countdown_banner);

        // ── Dropout banner (mid-ride) ────────────────────────────────────────
        let dropout_banner = adw::Banner::builder().revealed(false).build();
        root.append(&dropout_banner);

        // ── Cockpit layout ───────────────────────────────────────────────────
        // Everything the rider needs mid-effort is visible at once — no scroll.
        // The graph absorbs spare height so the page fills any window size.
        let clamp = adw::Clamp::builder()
            .maximum_size(WINDOWED_CLAMP)
            .margin_top(18)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .vexpand(true)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        // The cockpit above and below the graph's slot, each one section, so
        // the road can have the whole screen in fullscreen — see
        // `route_immersion`.
        let above = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .vexpand(true)
            .build();
        let below = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // ── Header row: workout name + connected device chips ────────────────
        let header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let workout_name_label = gtk::Label::builder()
            .label(&workout.name)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title-3"])
            .build();
        header_row.append(&workout_name_label);

        let devices_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .visible(false)
            .halign(gtk::Align::End)
            .build();
        devices_section.append(
            &gtk::Label::builder()
                .label("Connected:")
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let devices_flow = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        devices_section.append(&devices_flow);
        header_row.append(&devices_section);
        above.append(&header_row);

        // ── Hero row: target | live power | interval countdown ───────────────
        // Power is the page's reason to exist — it gets the largest class in the
        // cockpit scale and the centre column; the machine-set target and the
        // interval countdown flank it.
        //
        // This row and the secondary one below both `vexpand`, so spare height
        // flows into the numbers. It used to flow into the graph, which was the
        // only expanding child: on a large window the chart grew to nearly half
        // the page while the numbers stayed at their minimum, which is backwards
        // for a page read at arm's length mid-effort.
        let hero_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .vexpand(true)
            .valign(gtk::Align::Center)
            .build();

        let (target_box, target_label) = metric_column(
            "Target",
            Some("W"),
            "—",
            &["cockpit-major", "numeric", "accent"],
            POWER_DIGITS,
        );
        // The dial only when it is off the plan: at 100 % the percentage is
        // noise, and off it the target alone would not explain why it no longer
        // matches the graph. On its own line so it cannot widen the number.
        let target_intensity = gtk::Label::builder()
            .label("")
            .visible(false)
            .css_classes(["caption", "dim-label", "numeric"])
            .build();
        target_box.append(&target_intensity);
        hero_grid.attach(&target_box, 0, 0, 1, 1);

        let (power_box, power_label) = metric_column(
            "Power",
            Some("W"),
            "—",
            &["cockpit-hero", "numeric"],
            POWER_DIGITS,
        );
        hero_grid.attach(&power_box, 1, 0, 1, 1);

        let interval_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .build();
        let interval_caption = gtk::Label::builder()
            .label("Interval")
            .css_classes(["caption", "dim-label"])
            .build();
        let interval_label = gtk::Label::builder()
            .label("—")
            .width_chars(CLOCK_DIGITS)
            .css_classes(["cockpit-major", "numeric"])
            .build();
        // Full at the start of an interval and empty as it runs out, so the bar
        // and the number above it count the same way. Filling up while the
        // clock counts down had them disagreeing at a glance.
        let segment_progress = gtk::ProgressBar::builder().fraction(1.0).build();
        segment_progress.add_css_class("accent");
        interval_box.append(&interval_caption);
        interval_box.append(&interval_label);
        interval_box.append(&segment_progress);
        hero_grid.attach(&interval_box, 2, 0, 1, 1);
        above.append(&hero_grid);

        // ── Zone ribbon: where the current effort sits across Z1–Z7 ─────────
        let zone_meter = ZoneMeter::new(athlete.ftp_watts);
        above.append(zone_meter.widget());

        // ── Interval cue ─────────────────────────────────────────────────────
        // Sits directly under the hero numbers, where the rider is already
        // looking. Hidden until a ride is running and cues have been built, so
        // it never leaves an empty gap in the cockpit.
        let cue_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .visible(false)
            .build();
        let cue_headline = gtk::Label::builder()
            .label("")
            .css_classes(["cockpit-cue"])
            .justify(gtk::Justification::Center)
            .wrap(true)
            .build();
        let cue_detail = gtk::Label::builder()
            .label("")
            .css_classes(["cockpit-cue-detail", "dim-label"])
            .justify(gtk::Justification::Center)
            .wrap(true)
            .build();
        cue_box.append(&cue_headline);
        cue_box.append(&cue_detail);
        above.append(&cue_box);

        // ── Secondary metrics: HR · cadence · elapsed · remaining ────────────
        let secondary_grid = gtk::Grid::builder()
            .column_spacing(12)
            .column_homogeneous(true)
            .vexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let (hr_box, hr_label) = metric_column(
            "Heart Rate",
            Some("bpm"),
            "—",
            &["cockpit-metric", "numeric"],
            RATE_DIGITS,
        );
        let (cadence_box, cadence_label) = metric_column(
            "Cadence",
            Some("rpm"),
            "—",
            &["cockpit-metric", "numeric"],
            RATE_DIGITS,
        );
        let (elapsed_box, elapsed_label) = metric_column(
            "Elapsed",
            None,
            "0:00",
            &["cockpit-metric", "numeric"],
            CLOCK_DIGITS,
        );
        let (remaining_box, remaining_label, remaining_caption) = metric_column_parts(
            "Remaining",
            None,
            &WorkoutEngine::format_duration(workout.duration_secs),
            &["cockpit-metric", "numeric"],
            CLOCK_DIGITS,
        );
        secondary_grid.attach(&hr_box, 0, 0, 1, 1);
        secondary_grid.attach(&cadence_box, 1, 0, 1, 1);
        secondary_grid.attach(&elapsed_box, 2, 0, 1, 1);
        secondary_grid.attach(&remaining_box, 3, 0, 1, 1);
        above.append(&secondary_grid);

        // ── Workout power profile: a reference strip, not the page ───────────
        // A small floor and no `vexpand` — the inverse of the cockpit rule in
        // [`PlayerPage::set_fullscreen`]'s notes, and deliberately so. The
        // profile answers "what is coming", which is worth a glance between
        // efforts; the numbers answer "how am I doing right now", which is
        // worth the rest of the page. A fixed *floor* is safe where a fixed
        // *request* is not: it sets a minimum the page can always meet, and
        // never a minimum height the window has to grow to satisfy.
        let graph = WorkoutGraph::new(workout, athlete.ftp_watts);
        graph.widget().set_vexpand(false);
        graph.widget().set_size_request(-1, GRAPH_FLOOR_PX);

        // The road the workout is ridden along, in the graph's slot when the
        // rider asks for it. The graph keeps its floor; the road takes a share
        // of the spare height with the numbers, as the route page's does.
        let course_view = CourseView::new();
        course_view.widget().set_size_request(-1, ROAD_FLOOR_PX);
        let graph_stack = gtk::Stack::builder()
            .vhomogeneous(false)
            .interpolate_size(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        graph_stack.add_named(graph.widget(), Some("graph"));
        graph_stack.add_named(course_view.widget(), Some("3d"));
        inner.append(&above);
        inner.append(&graph_stack);
        inner.append(&below);

        // ── Whole-workout progress bar ───────────────────────────────────────
        let workout_progress = gtk::ProgressBar::builder()
            .fraction(0.0)
            .show_text(false)
            .build();
        workout_progress.add_css_class("accent");
        below.append(&workout_progress);

        // ── Session totals strip ─────────────────────────────────────────────
        // Live running totals in one glanceable line; the full breakdown
        // belongs to the post-ride summary page.
        let totals_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .halign(gtk::Align::Center)
            .build();

        let make_total_pair = |name: &str, tooltip: Option<&str>| -> gtk::Label {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            let name_lbl = gtk::Label::builder()
                .label(name)
                .css_classes(["caption", "dim-label"])
                .build();
            let value_lbl = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .build();
            if let Some(tip) = tooltip {
                pair.set_tooltip_text(Some(tip));
            }
            pair.append(&name_lbl);
            pair.append(&value_lbl);
            totals_row.append(&pair);
            value_lbl
        };

        let avg_power_total = make_total_pair("Avg", Some("Average power"));
        let np_total = make_total_pair("NP", Some("Normalised power"));
        let if_total = make_total_pair("IF", Some("Intensity factor"));
        let tss_total = make_total_pair("TSS", Some("Training stress score"));
        let kj_total = make_total_pair("Energy", Some("Total work in kilojoules"));
        below.append(&totals_row);

        // ── Playback controls ────────────────────────────────────────────────
        // Cancel/Pause/Skip at leading edge; hexpand spacer pushes End Workout to
        // the trailing edge so the destructive action is visually separated per HIG.
        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .build();

        let cancel_btn = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat", "pill"])
            .tooltip_text("Cancel and return to dashboard")
            .build();
        let pause_btn = gtk::Button::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Pause workout")
            .css_classes(["circular", "flat"])
            .build();
        let skip_btn = gtk::Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .tooltip_text("Skip to next interval")
            .css_classes(["circular", "flat"])
            .build();
        let end_btn = gtk::Button::builder()
            .label("End Workout")
            .css_classes(["destructive-action", "pill"])
            .tooltip_text("End the current workout")
            .build();

        // ── Intensity dial ───────────────────────────────────────────────────
        // Scales every remaining target, so a session that is too much today can
        // be finished at 90 % rather than abandoned.
        let intensity_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .valign(gtk::Align::Center)
            .tooltip_text("Workout intensity — scales every power target")
            .build();
        let intensity_down_btn = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Lower workout intensity")
            .css_classes(["circular", "flat"])
            .build();
        let intensity_label = gtk::Label::builder()
            .label("100%")
            .width_chars(4)
            .css_classes(["caption", "numeric"])
            .build();
        let intensity_up_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Raise workout intensity")
            .css_classes(["circular", "flat"])
            .build();
        // The percentage is the dial's value, not decoration — screen readers get
        // it from the group rather than from three unrelated children.
        intensity_box.update_property(&[gtk::accessible::Property::Label(
            "Workout intensity, percent of plan",
        )]);
        intensity_box.append(&intensity_down_btn);
        intensity_box.append(&intensity_label);
        intensity_box.append(&intensity_up_btn);

        let controls_spacer = gtk::Box::builder().hexpand(true).build();

        // Graph or road: the same slot, either way round. Hidden until a
        // workout with a road to ride is loaded.
        let view_3d_btn = gtk::ToggleButton::builder()
            .label("3D")
            .tooltip_text("Ride the workout along a road that climbs with the intervals")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();

        // Fullscreen from the page itself. A rider clipped into the pedals is
        // not in a position to go looking for F11, and the menu it also lives in
        // is three widgets away at the top of the window.
        let fullscreen_btn = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text("Fullscreen")
            .css_classes(["circular", "flat"])
            .action_name("win.toggle-fullscreen")
            .build();

        // Opens the compact window that can be pinned over a film. Its own
        // header explains the pinning, which this app is not allowed to do
        // itself — see `crate::ui::overlay`.
        let overlay_btn = gtk::Button::builder()
            .icon_name("picture-in-picture-symbolic")
            .tooltip_text("Open the compact overlay window")
            .css_classes(["circular", "flat"])
            .action_name("win.toggle-overlay")
            .build();

        controls.append(&cancel_btn);
        controls.append(&pause_btn);
        controls.append(&skip_btn);
        controls.append(&intensity_box);
        controls.append(&controls_spacer);
        controls.append(&view_3d_btn);
        controls.append(&overlay_btn);
        controls.append(&fullscreen_btn);
        controls.append(&end_btn);
        inner.append(&controls);

        let immersion = Immersion::new(Parts {
            clamp: clamp.clone(),
            inner: inner.clone(),
            windowed_top: 18,
            around: vec![above.upcast(), below.upcast()],
            controls: controls.clone(),
            course_view: course_view.clone(),
        });
        {
            let graph_stack = graph_stack.clone();
            view_3d_btn.connect_toggled(move |btn| {
                let road = btn.is_active();
                graph_stack.set_visible_child_name(if road { "3d" } else { "graph" });
                // The graph is a strip; the road shares the spare height.
                graph_stack.set_vexpand(road);
            });
        }
        view_3d_btn.connect_toggled(Immersion::road_toggle(&immersion));

        clamp.set_child(Some(&inner));
        root.append(&clamp);

        // ── Wire buttons once through replaceable callbacks ───────────────────
        // Each start_timer call replaces the callbacks; the buttons are never
        // connected more than once, preventing handler accumulation.
        let end_cb: ButtonCb = Rc::new(RefCell::new(None));
        let pause_cb: ButtonCb = Rc::new(RefCell::new(None));
        let skip_cb: ButtonCb = Rc::new(RefCell::new(None));
        let cancel_cb: ButtonCb = Rc::new(RefCell::new(None));
        let intensity_down_cb: ButtonCb = Rc::new(RefCell::new(None));
        let intensity_up_cb: ButtonCb = Rc::new(RefCell::new(None));
        let overlay_cb: ButtonCb = Rc::new(RefCell::new(None));

        {
            let cb = Rc::clone(&end_cb);
            end_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }
        {
            let cb = Rc::clone(&pause_cb);
            pause_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }
        {
            let cb = Rc::clone(&skip_cb);
            skip_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }
        {
            let cb = Rc::clone(&cancel_cb);
            cancel_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }
        {
            let cb = Rc::clone(&intensity_down_cb);
            intensity_down_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }
        {
            let cb = Rc::clone(&intensity_up_cb);
            intensity_up_btn.connect_clicked(move |_| {
                crate::ui::call_reload(&cb);
            });
        }

        Self::add_intensity_shortcuts(&root, &intensity_down_cb, &intensity_up_cb);

        Self {
            root,
            power_label,
            hr_label,
            cadence_label,
            target_label,
            target_intensity,
            elapsed_label,
            remaining_label,
            remaining_caption,
            interval_label,
            workout_progress,
            segment_progress,
            interval_caption,
            zone_meter,
            countdown_banner,
            dropout_banner,
            devices_section,
            devices_flow,
            graph,
            avg_power_total,
            np_total,
            if_total,
            tss_total,
            kj_total,
            pause_btn,
            end_btn,
            last_readings: Rc::new(RefCell::new(ReadingsTracker::default())),
            power_countdown: Rc::new(Cell::new(0)),
            start_now_cb,
            cancel_btn,
            overlay: Rc::new(RefCell::new(None)),
            overlay_cb,
            end_cb,
            pause_cb,
            skip_cb,
            cancel_cb,
            intensity_label,
            intensity_down_cb,
            intensity_up_cb,
            workout_name_label,
            cue_box,
            cue_headline,
            cue_detail,
            cues: RefCell::new(Vec::new()),
            cues_enabled: Cell::new(true),
            cue_rendered: Cell::new(None),
            cue_generation: Cell::new(0),
            test_mode: Cell::new(workout.is_ramp_test()),
            ladder_steps: Cell::new(ladder_steps(workout)),
            immersion,
            fullscreen_btn,
            course_view,
            view_3d_btn,
            road: RefCell::new(None),
            restoring_road: Rc::new(Cell::new(false)),
        }
    }

    /// Lay the cockpit out for the room fullscreen gives it.
    ///
    /// Windowed, the 900 px clamp keeps line lengths sane next to the sidebar.
    /// Fullscreen there is no sidebar and no header, and holding the same
    /// 900 px would leave a ride marooned in the middle of a 27-inch screen, so
    /// the clamp opens up and the margins tighten onto the 6 px grid.
    /// Put the cockpit into (or out of) FTP-test mode for `workout`.
    ///
    /// A ramp test is ridden to exhaustion rather than completed, so two things
    /// on the page would otherwise lie about it: a countdown to an end that is
    /// not how the test finishes, and an end button that calls finishing the
    /// test "ending the workout". The cockpit counts ladder steps instead, and
    /// the button says what pressing it does.
    ///
    /// Every class is listed in both branches: setting `css_classes` replaces
    /// the whole set, so a partial list silently drops `pill`.
    fn set_test_mode(&self, workout: &Workout) {
        let testing = workout.is_ramp_test();
        self.test_mode.set(testing);
        self.ladder_steps.set(ladder_steps(workout));

        if testing {
            self.remaining_caption.set_label(&caption_text(
                "Step",
                Some(&format!("of {}", self.ladder_steps.get())),
            ));
            self.end_btn.set_label("I'm Done");
            self.end_btn.set_tooltip_text(Some(
                "Finish the test and read your FTP off your best minute",
            ));
            self.end_btn.set_css_classes(&["suggested-action", "pill"]);
        } else {
            self.remaining_caption
                .set_label(&caption_text("Remaining", None));
            self.end_btn.set_label("End Workout");
            self.end_btn
                .set_tooltip_text(Some("End the current workout"));
            self.end_btn
                .set_css_classes(&["destructive-action", "pill"]);
        }
    }

    /// Whether the ride in the cockpit is an FTP test. Read by `start_timer`,
    /// which words its confirmation accordingly.
    pub fn is_test_mode(&self) -> bool {
        self.test_mode.get()
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        crate::ui::dress_fullscreen_button(&self.fullscreen_btn, fullscreen);
        // Wider with tighter gaps on the cockpit, so the graph gains what the
        // header bar and the margins gave up and it still fits without
        // scrolling; the whole screen on the 3D road.
        Immersion::set_fullscreen(&self.immersion, fullscreen);
    }

    /// Lay out `road` for the workout just loaded, looking as `look`, and put
    /// the 3D button where the rider last left it (`shown`). With no road the
    /// button goes and the graph stays.
    pub fn set_world(
        &self,
        workout: &Workout,
        road: Option<WorkoutRoad>,
        look: WorldLook,
        shown: bool,
    ) {
        if let Some(r) = &road {
            self.course_view.set_route(&r.route, look);
            self.course_view
                .set_workout_profile(ride_hud::workout_profile(workout));
        }
        self.view_3d_btn.set_visible(road.is_some());
        *self.road.borrow_mut() = road;
        self.restoring_road.set(true);
        self.view_3d_btn
            .set_active(shown && self.view_3d_btn.is_visible());
        self.restoring_road.set(false);
    }

    /// Run `save` with the rider's choice whenever they switch between the
    /// graph and the road — but not when [`PlayerPage::set_world`] restores it.
    pub fn connect_road_chosen(&self, save: impl Fn(bool) + 'static) {
        let restoring = Rc::clone(&self.restoring_road);
        self.view_3d_btn.connect_toggled(move |btn| {
            if !restoring.get() {
                save(btn.is_active());
            }
        });
    }

    /// Bind `+` and `−` (and their keypad twins) to the intensity dial.
    ///
    /// Global scope with a mapped check, like the library's Ctrl+F
    /// (`ui/pages/library/mod.rs`): the rider reaches this page by starting a
    /// workout, so focus is not on it and a page-local shortcut would never fire.
    fn add_intensity_shortcuts(root: &gtk::Box, down_cb: &ButtonCb, up_cb: &ButtonCb) {
        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);

        for (keys, cb) in [("minus|KP_Subtract", down_cb), ("plus|equal|KP_Add", up_cb)] {
            let Some(trigger) = gtk::ShortcutTrigger::parse_string(keys) else {
                tracing::warn!("Could not parse intensity shortcut '{keys}'");
                continue;
            };
            let cb = Rc::clone(cb);
            let action = gtk::CallbackAction::new(move |widget, _| {
                if !widget.is_mapped() {
                    return glib::Propagation::Proceed;
                }
                crate::ui::call_reload(&cb);
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
        }

        root.add_controller(controller);
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Merge incoming readings into the stored state.
    pub fn set_readings(&self, readings: LiveReadings) {
        self.last_readings
            .borrow_mut()
            .merge(readings, std::time::Instant::now());
    }

    /// Raise or clear the dropout banner for the sensors that have gone quiet.
    ///
    /// Driven by sensor staleness rather than by BLE connection events, so it
    /// also catches a device that is still nominally connected but has stopped
    /// sending — which looks identical to the rider.
    pub fn set_stale_sensors(&self, stale: &[&'static str]) {
        match crate::ui::dropout_banner_text(stale) {
            Some(title) => {
                self.dropout_banner.set_title(&title);
                self.dropout_banner.set_revealed(true);
            }
            None => self.dropout_banner.set_revealed(false),
        }
    }

    /// Add a connected-device chip to the status row.
    pub fn add_connected_device(&self, address: &str, display_name: &str) {
        // Guard against duplicates
        let mut child = self.devices_flow.first_child();
        while let Some(c) = child {
            if c.widget_name() == address {
                return;
            }
            child = c.next_sibling();
        }

        let chip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        chip.add_css_class("card");
        chip.set_widget_name(address);
        chip.append(
            &gtk::Image::builder()
                .icon_name("bluetooth-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        chip.append(
            &gtk::Label::builder()
                .label(display_name)
                .css_classes(["caption"])
                .build(),
        );

        self.devices_flow.append(&chip);
        self.devices_section.set_visible(true);
    }

    /// Remove a connected-device chip by address.
    pub fn remove_connected_device(&self, address: &str) {
        let mut child = self.devices_flow.first_child();
        while let Some(c) = child {
            let next = c.next_sibling();
            if c.widget_name() == address {
                self.devices_flow.remove(&c);
                break;
            }
            child = next;
        }
        if self.devices_flow.first_child().is_none() {
            self.devices_section.set_visible(false);
        }
    }

    /// Reset all UI for a new workout without rebuilding the widget tree.
    pub fn reset_workout(&self, workout: &Workout, ftp_watts: u32) {
        self.set_test_mode(workout);
        self.graph.set_ftp(ftp_watts);
        self.zone_meter.set_ftp(ftp_watts);
        self.workout_name_label.set_label(&workout.name);
        self.cancel_btn.set_visible(true);
        self.countdown_banner
            .set_title("Connect a power meter or trainer to begin");
        self.countdown_banner.set_revealed(true);
        // A ride that ended while auto-paused leaves the banner saying
        // "Resume Now"; the next one has nothing to resume.
        self.countdown_banner.set_button_label(Some("Start Now"));
        self.dropout_banner.set_revealed(false);
        self.power_countdown.set(0);
        // Cues belong to one workout. Clearing them here means a ride whose
        // history has not loaded yet shows nothing rather than the last ride's
        // rep numbers, which would be wrong for a different workout.
        self.cues.replace(Vec::new());
        self.cue_rendered.set(None);
        self.cue_box.set_visible(false);
        self.cue_generation
            .set(self.cue_generation.get().wrapping_add(1));
        self.graph.set_workout(workout);
        // The last workout's road is not this one's; `load_world` lays the new
        // one out.
        *self.road.borrow_mut() = None;
        self.zone_meter.set_power(None);
        self.elapsed_label.set_label("0:00");
        // No step has been reached yet in a test; the warm-up comes first.
        let remaining = if self.test_mode.get() {
            "—".to_string()
        } else {
            WorkoutEngine::format_duration(workout.duration_secs)
        };
        self.remaining_label.set_label(&remaining);
        self.workout_progress.set_fraction(0.0);
        // Full, not empty: the interval bar drains, so a fresh one is untouched.
        self.segment_progress.set_fraction(1.0);
        self.power_label.set_label("—");
        self.hr_label.set_label("—");
        self.cadence_label.set_label("—");
        self.target_label.set_label("—");
        self.target_intensity.set_visible(false);
        // The engine resets the dial for a new workout; the label must follow.
        self.intensity_label.set_label("100%");
        self.interval_label.set_label("—");
        self.interval_caption.set_label("Interval");
        self.avg_power_total.set_label("—");
        self.np_total.set_label("—");
        self.if_total.set_label("—");
        self.tss_total.set_label("—");
        self.kj_total.set_label("—");
    }

    /// Invoke the pause/resume toggle programmatically.
    ///
    /// The pause/resume button toggles based on the engine state, so the caller is
    /// responsible for only triggering this when a state change is wanted — e.g. the
    /// header "Resume Workout" button calls it only when the engine is paused, so it
    /// never accidentally pauses a running workout.
    pub fn trigger_pause_toggle(&self) {
        crate::ui::call_reload(&self.pause_cb);
    }

    /// Start the 1 Hz GLib timer, wire the playback controls, and call `on_complete`
    /// (exactly once) when the workout finishes.
    ///
    /// The engine starts automatically after 10 consecutive seconds of power data.
    /// The countdown banner's "Start Now" button bypasses the wait.
    pub fn start_timer(
        page: Rc<RefCell<Self>>,
        engine: Rc<RefCell<WorkoutEngine>>,
        on_complete: impl Fn(Session) + 'static,
        on_cancel: impl Fn() + 'static,
        timer_alive: Rc<Cell<bool>>,
        auto_pause_on: Rc<Cell<bool>>,
    ) {
        timer_alive.set(true);
        let on_complete: Rc<dyn Fn(Session)> = Rc::new(on_complete);
        let completed = Rc::new(Cell::new(false));

        // ── Wire Cancel button (Idle-state escape hatch) ─────────────────────
        {
            let engine_cancel = Rc::clone(&engine);
            let timer_alive_cancel = Rc::clone(&timer_alive);
            *page.borrow().cancel_cb.borrow_mut() = Some(Rc::new(move || {
                if engine_cancel.borrow().state == EngineState::Idle {
                    timer_alive_cancel.set(false);
                    on_cancel();
                }
            }));
        }

        // ── Wire "Start Now" button ──────────────────────────────────────────
        // The same banner says "Resume Now" while auto-paused. `start` on a
        // paused engine would re-stamp the ride's start time, so a paused ride
        // goes through the pause button's own path instead.
        {
            let engine_start_now = Rc::clone(&engine);
            let power_countdown_now = Rc::clone(&page.borrow().power_countdown);
            let countdown_banner_now = page.borrow().countdown_banner.clone();
            let pause_cb_now = Rc::clone(&page.borrow().pause_cb);
            *page.borrow().start_now_cb.borrow_mut() = Some(Rc::new(move || {
                let state = engine_start_now.borrow().state;
                match state {
                    EngineState::Idle => engine_start_now.borrow_mut().start(),
                    EngineState::Paused => crate::ui::call_reload(&pause_cb_now),
                    _ => {}
                }
                power_countdown_now.set(0);
                countdown_banner_now.set_revealed(false);
            }));
        }

        // ── Pause / resume ───────────────────────────────────────────────────
        {
            let pause_btn = page.borrow().pause_btn.clone();
            let engine_pause = Rc::clone(&engine);
            *page.borrow().pause_cb.borrow_mut() = Some(Rc::new(move || {
                let mut eng = engine_pause.borrow_mut();
                match eng.state {
                    EngineState::Running => {
                        eng.pause();
                        pause_btn.set_icon_name("media-playback-start-symbolic");
                        pause_btn.set_tooltip_text(Some("Resume workout"));
                    }
                    EngineState::Paused => {
                        eng.resume();
                        pause_btn.set_icon_name("media-playback-pause-symbolic");
                        pause_btn.set_tooltip_text(Some("Pause workout"));
                    }
                    _ => {}
                }
            }));
        }

        // ── Skip to next interval ────────────────────────────────────────────
        {
            let engine_skip = Rc::clone(&engine);
            *page.borrow().skip_cb.borrow_mut() = Some(Rc::new(move || {
                engine_skip.borrow_mut().skip_to_next_segment();
            }));
        }

        // ── Intensity dial ───────────────────────────────────────────────────
        // The label is set here as well as from the snapshot: waiting up to a
        // second for the tick to catch up reads as a dead button.
        let dial_cb = |delta: i32| -> Rc<dyn Fn()> {
            let engine_dial = Rc::clone(&engine);
            let label = page.borrow().intensity_label.clone();
            Rc::new(move || {
                let pct = {
                    let mut eng = engine_dial.borrow_mut();
                    eng.adjust_intensity(delta);
                    eng.intensity_pct()
                };
                label.set_label(&format!("{pct}%"));
            })
        };
        *page.borrow().intensity_down_cb.borrow_mut() = Some(dial_cb(-INTENSITY_STEP_PCT));
        *page.borrow().intensity_up_cb.borrow_mut() = Some(dial_cb(INTENSITY_STEP_PCT));

        // ── End Workout ──────────────────────────────────────────────────────
        {
            let end_btn = page.borrow().end_btn.clone();
            let engine_end = Rc::clone(&engine);
            let on_complete_end = Rc::clone(&on_complete);
            let completed_end = Rc::clone(&completed);
            let testing = page.borrow().is_test_mode();
            *page.borrow().end_cb.borrow_mut() = Some(Rc::new(move || {
                // Kept in test mode too: a mis-tap part-way up the ladder would
                // throw away a test that cannot be resumed. But it is finishing
                // the test, not abandoning a session, so it is not destructive
                // and it does not call itself ending a workout.
                let dialog = if testing {
                    adw::AlertDialog::builder()
                        .heading("Finished the Test?")
                        .body(
                            "Tour du Salon will read your FTP off your best minute and \
                             offer you the number.",
                        )
                        .build()
                } else {
                    adw::AlertDialog::builder()
                        .heading("End Workout?")
                        .body("Your progress so far will be saved.")
                        .build()
                };
                dialog.add_response("cancel", "_Cancel");
                if testing {
                    dialog.add_response("end", "_I'm Done");
                    dialog.set_response_appearance("end", adw::ResponseAppearance::Suggested);
                } else {
                    dialog.add_response("end", "_End Workout");
                    dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
                }
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");

                let engine_resp = Rc::clone(&engine_end);
                let on_complete_resp = Rc::clone(&on_complete_end);
                let completed_resp = Rc::clone(&completed_end);
                dialog.connect_response(None, move |_, response| {
                    if response == "end" && !completed_resp.get() {
                        completed_resp.set(true);
                        let session = {
                            let mut eng = engine_resp.borrow_mut();
                            eng.stop();
                            eng.session.clone()
                        };
                        on_complete_resp(session);
                    }
                });
                dialog.present(Some(&end_btn));
            }));
        }

        // ── 1 Hz tick loop ───────────────────────────────────────────────────
        let last_readings_rc = Rc::clone(&page.borrow().last_readings);
        // Sensors already reported as dropped, so each is logged once per ride.
        let stale_warned: Rc<RefCell<std::collections::HashSet<&'static str>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        let power_countdown = Rc::clone(&page.borrow().power_countdown);
        let auto_pause = RefCell::new(AutoPause::default());
        let page_clone = Rc::clone(&page);
        let engine_clone = Rc::clone(&engine);
        let on_complete_timer = Rc::clone(&on_complete);
        let completed_timer = Rc::clone(&completed);
        let timer_alive_in_timer = Rc::clone(&timer_alive);

        glib::timeout_add_local(Duration::from_secs(1), move || {
            if !timer_alive_in_timer.get() {
                page_clone.borrow().close_overlay();
                return glib::ControlFlow::Break;
            }
            // Only sensors still transmitting contribute — a strap that has gone
            // quiet must not have its last value recorded for the rest of the ride.
            let now = std::time::Instant::now();
            let readings = last_readings_rc.borrow().current(now);
            let stale = last_readings_rc.borrow().stale_sensors(now);
            for sensor in &stale {
                if stale_warned.borrow_mut().insert(*sensor) {
                    tracing::warn!("{sensor} sensor stopped reporting — dropping it from the ride");
                }
            }
            page_clone.borrow().set_stale_sensors(&stale);

            let (snapshot, session, ftp) = {
                let mut eng = engine_clone.borrow_mut();

                // Auto-start: count consecutive seconds of pedalling, and start
                // the engine once there are enough of them.
                if eng.state == EngineState::Idle {
                    if is_pedalling(readings.power_watts) {
                        let n = power_countdown.get() + 1;
                        power_countdown.set(n);
                        if n >= PRE_START_SECS {
                            eng.start();
                            power_countdown.set(0);
                        }
                    } else {
                        power_countdown.set(0);
                    }
                }

                let snapshot = eng.tick(readings);
                let session = eng.session.clone();
                let ftp = eng.athlete.borrow().ftp_watts;
                (snapshot, session, ftp)
            };

            // Update countdown banner title while still waiting.
            if snapshot.state == EngineState::Idle {
                let n = power_countdown.get();
                let title = if n == 0 {
                    "Start pedalling to begin".to_string()
                } else {
                    format!("Starting in {} s…", PRE_START_SECS.saturating_sub(n))
                };
                page_clone.borrow().countdown_banner.set_title(&title);
            }

            // Auto-pause acts through the pause button's own callback, so the
            // icon, the paused time and the trainer release all follow as they
            // would for a press. The engine borrow above is already released.
            // A ramp test opts out: it ends when the rider can no longer turn
            // the pedals, and that must read as the end, not a pause.
            let ride_state = match snapshot.state {
                EngineState::Running => RideState::Running,
                EngineState::Paused => RideState::Paused,
                _ => RideState::NotRiding,
            };
            let enabled = auto_pause_on.get() && !page_clone.borrow().is_test_mode();
            let action =
                auto_pause
                    .borrow_mut()
                    .tick(ride_state, snapshot.readings.power_watts, enabled);
            if action != auto_pause::Action::None {
                page_clone.borrow().trigger_pause_toggle();
            }
            let pause_status = {
                let ap = auto_pause.borrow();
                if ap.is_auto_paused() {
                    Some(auto_pause::banner_title(ap.pedalling_for()))
                } else if engine_clone.borrow().state == EngineState::Paused {
                    Some("Paused".to_string())
                } else {
                    None
                }
            };
            let auto_paused = auto_pause.borrow().is_auto_paused();

            let p = page_clone.borrow();
            p.update_from_snapshot(&snapshot);
            // The pre-start banner doubles as the auto-pause one. A pause the
            // rider pressed needs no banner — the button they pressed says it.
            if auto_paused {
                if let Some(title) = &pause_status {
                    p.countdown_banner.set_title(title);
                }
                p.countdown_banner.set_button_label(Some("Resume Now"));
                p.countdown_banner.set_revealed(true);
            } else {
                p.countdown_banner.set_button_label(Some("Start Now"));
            }
            p.update_session_totals(&session, ftp);
            p.graph.record_live(&snapshot);
            p.ride_the_road(&snapshot, ftp);
            // The overlay rides this tick rather than one of its own, so the two
            // windows can never show different seconds of the same ride.
            if let Some(overlay) = p.overlay.borrow().as_ref() {
                overlay.update(&snapshot, &stale);
                overlay.set_pause_status(pause_status.as_deref());
            }
            drop(p);

            if snapshot.state == EngineState::Completed {
                if !completed_timer.get() {
                    completed_timer.set(true);
                    // The ride is over, so the overlay has nothing left to say.
                    page_clone.borrow().close_overlay();
                    on_complete_timer(session);
                }
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    /// Open the compact overlay, or close it if it is already open.
    ///
    /// The work of building it lives in the callback `attach_overlay` installs,
    /// which is the only place that knows the database and the application.
    pub fn toggle_overlay(&self) {
        let is_open = self.overlay_is_open();
        if is_open {
            self.close_overlay();
            return;
        }
        crate::ui::call_reload(&self.overlay_cb);
    }

    /// Whether the compact overlay window is open.
    pub fn overlay_is_open(&self) -> bool {
        self.overlay.borrow().is_some()
    }

    /// Close the overlay if it is open, and forget it.
    ///
    /// The overlay is taken out of its cell *before* the window is closed:
    /// closing fires the close handler, which clears the same cell, and holding
    /// the borrow across that would panic (CLAUDE.md §2.4).
    pub fn close_overlay(&self) {
        let overlay = self.overlay.borrow_mut().take();
        if let Some(overlay) = overlay {
            overlay.close();
        }
    }

    pub fn update_from_snapshot(&self, snap: &EngineSnapshot) {
        // The units live in the captions (see `metric_column`), so a reading is
        // its digits and nothing else.
        self.power_label.set_label(
            &snap
                .readings
                .power_watts
                .map(|w| w.to_string())
                .unwrap_or_else(|| "—".into()),
        );
        self.hr_label.set_label(
            &snap
                .readings
                .heart_rate_bpm
                .map(|h| h.to_string())
                .unwrap_or_else(|| "—".into()),
        );
        self.cadence_label.set_label(
            &snap
                .readings
                .cadence_rpm
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".into()),
        );
        self.target_label
            .set_label(&snap.target_power_watts.to_string());
        self.target_intensity
            .set_label(&format!("{}% of plan", snap.intensity_pct));
        self.target_intensity.set_visible(snap.intensity_pct != 100);
        self.intensity_label
            .set_label(&format!("{}%", snap.intensity_pct));
        self.zone_meter.set_power(snap.readings.power_watts);

        self.elapsed_label
            .set_label(&WorkoutEngine::format_duration(snap.elapsed_secs));
        if self.test_mode.get() {
            // Which step is being ridden, not how many are left: the ladder runs
            // far past where anyone finishes, so "18 to go" tells the rider
            // nothing. Just the number, because the total lives in the caption
            // and the column reserves a clock's width, not a fraction's.
            self.remaining_label.set_label(
                &step_number(snap.segment_index, self.ladder_steps.get())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".into()),
            );
        } else {
            self.remaining_label
                .set_label(&WorkoutEngine::format_duration(snap.remaining_secs));
        }
        self.interval_label
            .set_label(&WorkoutEngine::format_duration(snap.segment_remaining_secs));

        let total = snap.elapsed_secs + snap.remaining_secs;
        if total > 0 {
            self.workout_progress
                .set_fraction(snap.elapsed_secs as f64 / total as f64);
        }
        // Drains: the bar empties as the interval's clock above it runs down.
        let seg_total = snap.segment_elapsed_secs + snap.segment_remaining_secs;
        if seg_total > 0 {
            self.segment_progress
                .set_fraction(snap.segment_remaining_secs as f64 / seg_total as f64);
        }

        self.interval_caption
            .set_label(&self.interval_caption_text(snap));

        self.update_cue(snap);

        // Countdown banner and cancel button: visible only while engine is still Idle.
        let is_idle = snap.state == EngineState::Idle;
        self.countdown_banner.set_revealed(is_idle);
        self.cancel_btn.set_visible(is_idle);
    }

    /// What the interval clock is captioned: numbered while the workout is
    /// active, and in a test the step — the warm-up is not a step, so it is
    /// named rather than numbered.
    fn interval_caption_text(&self, snap: &EngineSnapshot) -> String {
        let is_active = matches!(snap.state, EngineState::Running | EngineState::Paused);
        if self.test_mode.get() {
            step_number(snap.segment_index, self.ladder_steps.get())
                .filter(|_| is_active)
                .map_or_else(|| "Warm-up".to_string(), |n| format!("Step {n}"))
        } else if is_active {
            format!("Interval {}", snap.segment_index + 1)
        } else {
            "Interval".to_string()
        }
    }

    /// Move the rider along the workout's road to where the clock has got
    /// to, and give the road's HUD this second's readings. The road follows
    /// the clock, not the watts — see `data::workout_road` — so it stands
    /// still while the workout is paused or not yet started.
    fn ride_the_road(&self, snap: &EngineSnapshot, ftp_watts: u32) {
        let road = self.road.borrow();
        let Some(road) = road.as_ref() else {
            return;
        };
        let t = snap.elapsed_secs as f32;
        let speed = if snap.state == EngineState::Running {
            road.speed_at(t)
        } else {
            0.0
        };
        self.course_view.set_progress(road.distance_at(t), speed);
        let is_active = matches!(snap.state, EngineState::Running | EngineState::Paused);
        let total = snap.elapsed_secs + snap.remaining_secs;
        self.course_view.set_workout_hud(WorkoutHudReadings {
            workout_name: self.workout_name_label.label().to_string(),
            power_watts: snap.readings.power_watts,
            target_watts: is_active.then_some(snap.target_power_watts),
            ftp_watts,
            interval_caption: self.interval_caption_text(snap).to_uppercase(),
            interval_left_secs: is_active.then_some(snap.segment_remaining_secs),
            heart_rate_bpm: snap.readings.heart_rate_bpm,
            cadence_rpm: snap.readings.cadence_rpm,
            elapsed_secs: snap.elapsed_secs,
            remaining_secs: (!self.test_mode.get()).then_some(snap.remaining_secs),
            progress: if total > 0 {
                snap.elapsed_secs as f32 / total as f32
            } else {
                0.0
            },
        });
    }

    /// Show the cue for the segment being ridden.
    ///
    /// Called every tick, but only rewrites the labels when the segment changes
    /// or the interval enters its closing seconds — a cue that re-rendered each
    /// second would restyle a widget the rider is reading mid-effort.
    fn update_cue(&self, snap: &EngineSnapshot) {
        // Before the start the countdown banner is doing the talking, and after
        // the finish the summary page is; a cue in either case is noise.
        let riding = matches!(snap.state, EngineState::Running | EngineState::Paused)
            // A test has one instruction — keep going until you cannot — and it
            // is on the button. Rep-counting cues built for a plan would only
            // talk over it.
            && !self.test_mode.get();
        let cues = self.cues.borrow();
        let cue = cues.get(snap.segment_index);

        let Some(cue) = cue.filter(|_| riding && self.cues_enabled.get()) else {
            self.cue_box.set_visible(false);
            self.cue_rendered.set(None);
            return;
        };

        // The closing line only replaces the detail while the interval is still
        // running — at zero the engine has already moved to the next segment.
        let closing = cue.closing.is_some()
            && snap.segment_remaining_secs <= CLOSING_SECS
            && snap.segment_remaining_secs > 0;

        if self.cue_rendered.get() != Some((snap.segment_index, closing)) {
            self.cue_headline.set_label(&cue.headline);

            let detail = if closing {
                cue.closing.as_deref()
            } else {
                cue.detail.as_deref()
            };
            self.cue_detail.set_label(detail.unwrap_or(""));
            self.cue_detail.set_visible(detail.is_some());

            // An effort gets the accent colour so the rider can tell at a glance
            // whether they are working or resting; the closing seconds of one
            // get it too, on the line that is counting them down.
            self.cue_headline.set_css_classes(if cue.role.is_effort() {
                &["cockpit-cue", "accent"]
            } else {
                &["cockpit-cue"]
            });
            self.cue_detail.set_css_classes(if closing {
                &["cockpit-cue-detail", "heading", "accent"]
            } else {
                &["cockpit-cue-detail", "dim-label"]
            });

            self.cue_rendered.set(Some((snap.segment_index, closing)));
        }

        self.cue_box.set_visible(true);
    }

    /// Whether this ride's interval cues have been built yet.
    ///
    /// [`load_cues`] fills them from a database read, so a caller that needs
    /// them present — the offscreen review renders, which have no ride loop to
    /// wait in — has to be able to ask. Test-only, and gated: a `pub fn` a
    /// shipped build never calls is a clippy `dead_code` error, not a warning.
    #[cfg(test)]
    pub fn has_cues(&self) -> bool {
        !self.cues.borrow().is_empty()
    }

    /// Apply the Preferences → Training toggle, mid-ride if need be.
    pub fn set_cues_enabled(&self, enabled: bool) {
        self.cues_enabled.set(enabled);
        if !enabled {
            self.cue_box.set_visible(false);
            self.cue_rendered.set(None);
        }
    }

    /// Update the live session totals strip from the current in-progress session.
    pub fn update_session_totals(&self, session: &Session, ftp: u32) {
        self.zone_meter.set_ftp(ftp);
        // The page outlives any one ride, so the graph's FTP scale has to track
        // the profile rather than keep the value it was built with.
        self.graph.set_ftp(ftp);
        match session.average_power() {
            Some(p) => self.avg_power_total.set_label(&format!("{} W", p as u32)),
            None => self.avg_power_total.set_label("—"),
        }
        let np = session.normalised_power();
        match np {
            Some(p) => self.np_total.set_label(&format!("{} W", p as u32)),
            None => self.np_total.set_label("—"),
        }
        match np {
            Some(p) => self.if_total.set_label(&format!("{:.2}", p / ftp as f32)),
            None => self.if_total.set_label("—"),
        }
        match session.tss(ftp) {
            Some(t) => self.tss_total.set_label(&format!("{}", t as u32)),
            None => self.tss_total.set_label("—"),
        }
        let kj = session.kilojoules();
        self.kj_total.set_label(&format!("{:.0} kJ", kj));
    }
}

/// Build this workout's interval cues and hand them to the player.
///
/// Call straight after `reset_workout`. The rep numbering comes from the workout
/// itself, but the "you've ridden this before" line needs the rider's history,
/// which lives in the database — so the read runs on the tokio runtime and the
/// cues land on the main thread a moment later (CLAUDE.md §2.3). A ride never
/// waits for them; the cue row appears when they arrive.
///
/// A failed read is logged and the structural cues are built anyway. Cues are an
/// extra, and losing the history line is not worth interrupting a ride over.
/// Wire the player's overlay button to whatever ride the engine is holding.
///
/// Installed once at build time rather than per ride: the engine outlives every
/// individual workout, so reading the workout, FTP and session out of it at
/// click time keeps the overlay right across `reset_with_workout` without this
/// needing to be re-installed.
///
/// The closure is stored *inside* the page, so it must not hold anything that
/// owns the page (CLAUDE.md §2.4) — hence the weak application reference, and
/// the cloned overlay cell rather than the page itself.
/// Ladder steps in a ramp test, or 0 for anything else.
///
/// The warm-up and the cool-down bracket the ladder — see
/// [`Workout::ramp_test`].
fn ladder_steps(workout: &Workout) -> u32 {
    if !workout.is_ramp_test() {
        return 0;
    }
    workout
        .segments
        .len()
        .saturating_sub(crate::data::workout::RAMP_FIRST_STEP_INDEX + 1) as u32
}

/// Which ladder step a segment index is, counting from 1.
///
/// `None` during the warm-up, and past the last step for a rider who somehow
/// rode the whole ladder out into the cool-down.
fn step_number(segment_index: usize, ladder_steps: u32) -> Option<u32> {
    let first = crate::data::workout::RAMP_FIRST_STEP_INDEX;
    let step = segment_index.checked_sub(first)? as u32 + 1;
    (step <= ladder_steps).then_some(step)
}

pub fn attach_overlay(
    page: &Rc<RefCell<PlayerPage>>,
    engine: Rc<RefCell<WorkoutEngine>>,
    app: &adw::Application,
    pool: sqlx::SqlitePool,
    rt: tokio::runtime::Handle,
) {
    let overlay_cell = Rc::clone(&page.borrow().overlay);
    let cb: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        app,
        move || {
            // Already open: bring it forward rather than opening a second one.
            if let Some(open) = overlay_cell.borrow().as_ref() {
                open.present();
                return;
            }

            let (workout, ftp, session) = {
                let eng = engine.borrow();
                let ftp = eng.athlete.borrow().ftp_watts;
                (eng.workout.clone(), ftp, eng.session.clone())
            };

            let cell = Rc::clone(&overlay_cell);
            let pool_build = pool.clone();
            let rt_build = rt.clone();
            let pool_read = pool.clone();
            crate::ui::spawn_to_main(
                &rt,
                async move { settings::load_overlay(&pool_read).await },
                move |loaded| {
                    // A size and an opacity are cosmetic, so an unreadable
                    // database costs the rider their last choice rather than
                    // the window — but it is still said out loud.
                    let saved = loaded.unwrap_or_else(|e| {
                        tracing::error!("Could not read the overlay settings: {e}");
                        settings::OverlaySettings::default()
                    });

                    let overlay = RideOverlay::new(
                        app.upcast_ref::<gtk::Application>(),
                        &workout,
                        ftp,
                        saved,
                        pool_build.clone(),
                        rt_build.clone(),
                    );
                    overlay.backfill(&session, workout.duration_secs);

                    // Weak, or the window's own handler would keep the cell that
                    // holds the window alive for the life of the process.
                    let weak_cell = Rc::downgrade(&cell);
                    overlay.connect_closed(move || {
                        if let Some(cell) = weak_cell.upgrade() {
                            *cell.borrow_mut() = None;
                        }
                    });

                    overlay.present();

                    crate::ui::overlay::show_pin_help_once(
                        &app,
                        &overlay,
                        saved.hint_seen,
                        &pool_build,
                        &rt_build,
                    );

                    *cell.borrow_mut() = Some(overlay);
                },
            );
        }
    ));
    *page.borrow().overlay_cb.borrow_mut() = Some(cb);
}

pub fn load_cues(
    player: Rc<RefCell<PlayerPage>>,
    workout: Workout,
    pool: sqlx::SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    let generation = player.borrow().cue_generation.get();

    crate::ui::spawn_to_main(
        rt_handle,
        async move {
            let sessions = crate::data::db::load_session_summaries(&pool).await?;
            let activities = crate::data::db::load_intervals_activities(&pool).await?;
            anyhow::Ok((sessions, activities))
        },
        move |loaded| {
            let p = player.borrow();
            // The rider has started something else since; these cues are stale.
            if p.cue_generation.get() != generation {
                return;
            }

            let (sessions, activities) = match loaded {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Could not read ride history for interval cues: {e}");
                    (Vec::new(), Vec::new())
                }
            };

            let history = crate::training::progression::build_history(&sessions, &activities);
            let priors = crate::training::progression::prior_efforts(
                &history,
                &workout.name,
                chrono::Utc::now().date_naive(),
            );

            p.cues
                .replace(crate::training::cues::build_cues(&workout, &priors));
            p.cue_rendered.set(None);
        },
    );
}

/// Lay out the road `workout` is ridden along in 3D, in the scenery chosen in
/// Preferences, and put the 3D button where the rider left it. The settings
/// are read off the main thread; like [`load_cues`], a read that lands after
/// the rider has started something else is dropped.
pub fn load_world(
    player: Rc<RefCell<PlayerPage>>,
    workout: Workout,
    athlete: &AthleteProfile,
    pool: sqlx::SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    let generation = player.borrow().cue_generation.get();
    let road = WorkoutRoad::build(&workout, athlete);
    crate::ui::spawn_to_main(
        rt_handle,
        async move {
            let look = settings::workout_look(&pool).await?;
            let shown = settings::workout_road(&pool).await?;
            anyhow::Ok((look, shown))
        },
        move |loaded| {
            let p = player.borrow();
            if p.cue_generation.get() != generation {
                return;
            }
            let (look, shown) = loaded.unwrap_or_else(|e| {
                tracing::warn!("Could not read the 3D world settings: {e}");
                (WorldLook::default(), false)
            });
            p.set_world(&workout, road, look, shown);
        },
    );
}
