//! A compact ride window, for riding while watching something else.
//!
//! The rider wants a film on the same screen and still needs to know what to
//! push. This is the cockpit reduced to what can be read out of the corner of
//! an eye: what the plan asks for, what the legs are giving, how long this
//! effort lasts, and where the ride has got to.
//!
//! # It cannot put itself on top, and that is not a bug to fix
//!
//! On GNOME Wayland an application cannot raise its own window above others.
//! `gtk_window_set_keep_above()` was removed in GTK4; `wlr-layer-shell`, the
//! Wayland protocol that grants it, is implemented by wlroots, KWin and COSMIC
//! but not by Mutter; and no portal offers it either. Nothing written here can
//! change that.
//!
//! What the *rider* can do is pin any window: right-click this window's header
//! bar — or Super + right-click anywhere on it — and choose **Always on Top**.
//! [`pin_help_dialog`] says so on first open, and points at the unbound
//! `org.gnome.desktop.wm.keybindings always-on-top` key for anyone who would
//! rather press one key. It also asks for the film to be *maximised* rather
//! than fullscreen: Mutter stacks fullscreen windows in a layer of their own,
//! where an always-on-top window does not reliably win.
//!
//! # Why it is a second window
//!
//! Every other secondary window in this app is modal and transient for the main
//! one. This one is deliberately neither: transient-for would tie it to the main
//! window's stacking and minimised state, which is exactly what a window meant
//! to outlive its parent on screen must not do.

use adw::prelude::*;
use gtk::{gio, glib};

use crate::data::session::Session;
use crate::data::settings::{self, OverlayOpacity, OverlaySettings};
use crate::data::workout::Workout;
use crate::training::engine::{EngineSnapshot, WorkoutEngine};
use crate::ui::widgets::metric_column::{metric_column, CLOCK_DIGITS, POWER_DIGITS, RATE_DIGITS};
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::{dropout_banner_text, spawn_write};

/// Starting size, in pixels. Small enough to leave the film the screen, wide
/// enough for the reserved digit widths below not to fight for room.
const DEFAULT_WIDTH: i32 = 420;
const DEFAULT_HEIGHT: i32 = 312;

/// Height of the profile strip, in pixels (6 px grid).
///
/// The cockpit's floor is 180 px, which is most of this window. Here the graph
/// is a shape to glance at rather than something to read values off, so it gets
/// what is left once the numbers have had theirs.
const GRAPH_HEIGHT_PX: i32 = 72;

/// The opacity steps that are not the default, so switching between them can
/// clear whichever was applied last.
const OPACITY_CLASSES: [&str; 2] = ["semi", "faint"];

/// A live ride, reduced to a window that can sit over something else.
pub struct RideOverlay {
    window: adw::ApplicationWindow,
    power_label: gtk::Label,
    target_label: gtk::Label,
    interval_label: gtk::Label,
    hr_label: gtk::Label,
    cadence_label: gtk::Label,
    graph: WorkoutGraph,
    workout_progress: gtk::ProgressBar,
    dropout_label: gtk::Label,
}

impl RideOverlay {
    /// Build the overlay for a ride in progress.
    ///
    /// `saved` carries the size and opacity from last time; `pool` and `rt` are
    /// only ever used to write those back.
    pub fn new(
        app: &gtk::Application,
        workout: &Workout,
        ftp_watts: u32,
        saved: OverlaySettings,
        pool: sqlx::SqlitePool,
        rt: tokio::runtime::Handle,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(if workout.name.is_empty() {
                "Ride"
            } else {
                &workout.name
            })
            .default_width(DEFAULT_WIDTH)
            .default_height(DEFAULT_HEIGHT)
            .resizable(true)
            .build();
        // Added rather than set through the builder: `css-classes` replaces the
        // whole list, and libadwaita's own `.background` carries the theme's
        // foreground colour as well as its background. Dropping it left white
        // text on a light panel. This only overrides the background.
        window.add_css_class("ride-overlay");
        if let Some((w, h)) = saved.size() {
            window.set_default_size(w, h);
        }

        // The panel behind the numbers. The translucency lives here and not on
        // the window, so the text over it stays fully opaque — `set_opacity` on
        // the window would fade the readings along with the ground.
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["ride-overlay-body"])
            .build();
        apply_opacity(&body, saved.opacity);

        // ── Header ───────────────────────────────────────────────────────────
        // It earns its space twice over: it is the drag handle, and it is the
        // right-click target that reaches Mutter's Always-on-Top item.
        let header = adw::HeaderBar::builder()
            .css_classes(["flat"])
            .show_title(true)
            .build();

        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Overlay options")
            .menu_model(&build_menu())
            .build();
        header.pack_end(&menu_btn);

        let actions = gio::SimpleActionGroup::new();
        let opacity_action = gio::SimpleAction::new_stateful(
            "opacity",
            Some(glib::VariantTy::STRING),
            &saved.opacity.as_str().to_variant(),
        );
        {
            let pool = pool.clone();
            let rt = rt.clone();
            opacity_action.connect_activate(glib::clone!(
                #[weak]
                body,
                move |action, param| {
                    let Some(chosen) = param
                        .and_then(|p| p.str().map(str::to_owned))
                        .and_then(|name| OverlayOpacity::parse(&name))
                    else {
                        tracing::warn!("Ignoring an unrecognised overlay opacity");
                        return;
                    };
                    apply_opacity(&body, chosen);
                    action.set_state(&chosen.as_str().to_variant());
                    spawn_write(&rt, &pool, "the overlay opacity", move |pool| async move {
                        settings::set_overlay_opacity(&pool, chosen).await
                    });
                }
            ));
        }
        actions.add_action(&opacity_action);

        let help_action = gio::SimpleAction::new("pin-help", None);
        help_action.connect_activate(glib::clone!(
            #[weak]
            window,
            move |_, _| pin_help_dialog().present(Some(&window))
        ));
        actions.add_action(&help_action);
        window.insert_action_group("overlay", Some(&actions));

        // ── Row 1: what the legs are giving, and what the plan asks ──────────
        let power_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();
        let (power_box, power_label) = metric_column(
            "Power",
            Some("W"),
            "—",
            &["cockpit-major", "numeric"],
            POWER_DIGITS,
        );
        let (target_box, target_label) = metric_column(
            "Target",
            Some("W"),
            "—",
            &["cockpit-major", "numeric", "accent"],
            POWER_DIGITS,
        );
        power_row.append(&power_box);
        power_row.append(&target_box);

        // ── Row 2: how long this effort lasts, and the body's answer ─────────
        let detail_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();
        let (interval_box, interval_label) = metric_column(
            "To go",
            None,
            "—",
            &["cockpit-metric", "numeric"],
            CLOCK_DIGITS,
        );
        let (hr_box, hr_label) = metric_column(
            "HR",
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
        detail_row.append(&interval_box);
        detail_row.append(&hr_box);
        detail_row.append(&cadence_box);

        // ── The shape of the session, and where the ride has got to ──────────
        let graph = WorkoutGraph::new(workout, ftp_watts);
        // The cockpit's graph asks for 600×120. Left alone it would set this
        // window's minimum width to something wider than the window itself.
        graph.widget().set_content_width(0);
        graph.widget().set_content_height(GRAPH_HEIGHT_PX);

        // Built plain and classed afterwards, as the cockpit's bars are: setting
        // `css-classes` through the builder replaces the default list, and
        // GtkProgressBar's own `.horizontal` is what Adwaita hangs the trough's
        // min-height on. Through the builder it measured 402x0 and drew nothing.
        let workout_progress = gtk::ProgressBar::builder().fraction(0.0).build();
        workout_progress.add_css_class("accent");

        // Silent until a sensor stops reporting. It matters more here than on
        // the cockpit: the whole point of this window is that the rider is
        // looking at something else, so a number that has quietly frozen would
        // otherwise go on being believed.
        let dropout_label = gtk::Label::builder()
            .css_classes(["caption", "warning"])
            .wrap(true)
            .visible(false)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&power_row);
        content.append(&detail_row);
        content.append(graph.widget());
        content.append(&workout_progress);
        content.append(&dropout_label);

        body.append(&header);
        body.append(&content);
        window.set_content(Some(&body));

        // Remember the size, the way the main window does.
        window.connect_close_request(move |win| {
            let (width, height) = (win.width(), win.height());
            spawn_write(&rt, &pool, "the overlay size", move |pool| async move {
                settings::set_overlay_size(&pool, width, height).await
            });
            glib::Propagation::Proceed
        });

        Self {
            window,
            power_label,
            target_label,
            interval_label,
            hr_label,
            cadence_label,
            graph,
            workout_progress,
            dropout_label,
        }
    }

    /// The overlay's window, for presenting a dialog on it.
    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn close(&self) {
        self.window.close();
    }

    /// Run `on_closed` when the rider closes the window, so the page that owns
    /// this overlay can let go of it.
    pub fn connect_closed(&self, on_closed: impl Fn() + 'static) {
        self.window.connect_close_request(move |_| {
            on_closed();
            glib::Propagation::Proceed
        });
    }

    /// Fill the profile with a ride already under way.
    ///
    /// Without this the graph opens blank and only starts drawing from the
    /// moment the overlay did, which reads as a ride that has not happened yet.
    pub fn backfill(&self, session: &Session, total_secs: u32) {
        let mut trace: Vec<Option<u32>> = vec![None; total_secs as usize];
        for dp in &session.data_points {
            if let Some(slot) = trace.get_mut(dp.elapsed_secs as usize) {
                *slot = dp.power_watts;
            }
        }
        self.graph.set_trace(trace);
    }

    /// One tick's worth of numbers. Driven from the player's existing timer, so
    /// the two windows cannot disagree.
    pub fn update(&self, snap: &EngineSnapshot, stale: &[&'static str]) {
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
        self.interval_label
            .set_label(&WorkoutEngine::format_duration(snap.segment_remaining_secs));

        let total = snap.elapsed_secs + snap.remaining_secs;
        if total > 0 {
            self.workout_progress
                .set_fraction(snap.elapsed_secs as f64 / total as f64);
        }

        self.graph.set_playhead(snap.elapsed_secs);
        self.graph.push_power(snap.readings.power_watts);

        match dropout_banner_text(stale) {
            Some(text) => {
                self.dropout_label.set_label(&text);
                self.dropout_label.set_visible(true);
            }
            None => self.dropout_label.set_visible(false),
        }
    }
}

/// Swap the panel to one opacity step, clearing whichever was applied before.
fn apply_opacity(body: &gtk::Box, opacity: OverlayOpacity) {
    for class in OPACITY_CLASSES {
        body.remove_css_class(class);
    }
    if let Some(class) = opacity.css_class() {
        body.add_css_class(class);
    }
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let background = gio::Menu::new();
    background.append(Some("Solid"), Some("overlay.opacity::solid"));
    background.append(Some("Semi-transparent"), Some("overlay.opacity::semi"));
    background.append(Some("Faint"), Some("overlay.opacity::faint"));
    menu.append_section(Some("Background"), &background);

    menu.append(Some("Keep This on Top…"), Some("overlay.pin-help"));
    menu
}

/// How to pin the overlay, since the app is not allowed to do it.
///
/// Shown once on first use and available from the menu afterwards. See this
/// module's own documentation for why the app cannot do this itself.
pub fn pin_help_dialog() -> adw::AlertDialog {
    let dialog = adw::AlertDialog::builder()
        .heading("Keeping This Window on Top")
        .body(
            "GNOME does not let an app raise its own window, but you can pin \
             any window yourself:\n\n\
             1.  Right-click this window's header bar — or hold Super and \
             right-click anywhere on it — and choose “Always on Top”.\n\n\
             2.  Play your film in a maximised window rather than fullscreen. \
             A fullscreen window sits in a layer of its own, above even a \
             pinned window.\n\n\
             To make step 1 a single keystroke, run this once in a terminal:\n\n\
             gsettings set org.gnome.desktop.wm.keybindings always-on-top \
             \"['<Super>t']\"",
        )
        .build();
    dialog.add_response("ok", "_Got It");
    dialog.set_default_response(Some("ok"));
    dialog
}
