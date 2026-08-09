//! The dialogs behind a library row: what a workout or route actually is,
//! and the ways to act on it.

use adw::prelude::*;
use libshumate::prelude::LocationExt;
use sqlx::SqlitePool;
use std::rc::Rc;

use crate::data::db;
use crate::data::route::Route;
use crate::data::workout::Workout;
use crate::training::recommend::workout_fit;

#[allow(clippy::too_many_arguments)] // detail dialog wiring; grouping deferred
pub fn show_workout_detail(
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
            // AdwActionRow subtitles parse Pango markup, so the literal "<" must be
            // escaped or GTK treats "< 88%" as the start of an element.
            "Based on peak power as % of FTP — \
             Easy &lt; 88% · Moderate 88–110% · Hard 110–130% · Very Hard ≥ 130%",
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
    let fit = workout_fit(&workout, ctl, tsb, &goals);
    let fitness_group = adw::PreferencesGroup::builder().title("For You").build();
    let for_you_row = adw::ActionRow::builder()
        .title(&fit.form_text)
        .subtitle(&fit.rationale)
        .build();
    let for_you_icon = gtk::Image::builder()
        .icon_name(if fit.recommended {
            "starred-symbolic"
        } else {
            "dialog-information-symbolic"
        })
        .css_classes(if fit.recommended {
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
    let graph = WorkoutGraph::new(&workout, ftp);
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
        show_schedule_dialog(
            btn,
            workout_id,
            &workout_name,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&on_toast),
        );
    });

    let workout_for_start = workout.clone();
    // Weak: start_btn is inside this window (CLAUDE.md §2.4).
    start_btn.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| {
            on_start(workout_for_start.clone());
            win.close();
        }
    ));

    actions.append(&schedule_btn);
    actions.append(&start_btn);
    inner.append(&actions);

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

pub fn show_route_detail(
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
    // Weak: ride_btn is inside this window (CLAUDE.md §2.4).
    ride_btn.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| {
            on_start_route(route_for_ride.clone());
            win.close();
        }
    ));

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

/// Ask for a date, then put the workout on the calendar.
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
