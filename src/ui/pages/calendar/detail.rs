//! Detail views for a ride that has already happened.
//!
//! These were the History page before it was folded into the calendar, which is
//! now their only caller: activating a completed day opens one of them.

use adw::prelude::*;
use chrono::Local;
use libshumate::prelude::LocationExt;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use glib;
use gtk::gio;
use sqlx::SqlitePool;

use crate::data::athlete::power_zone_index;
use crate::data::db;
use crate::data::keystore;
use crate::data::settings;
use crate::data::sport::is_run;
use crate::data::streams::ActivityStreams;
use crate::data::workout::Workout;
use crate::training::analytics::{format_average_pace, format_distance};
use crate::training::engine::WorkoutEngine;
use crate::ui::widgets::zone_bar::ZoneBar;

use crate::ui::ReloadHolder;

pub fn show_intervals_detail(
    act: &db::IntervalsActivity,
    title: &str,
    ftp: u32,
    weight_kg: f32,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    parent: Option<&gtk::Window>,
) {
    let icu_id = act.icu_id.clone();

    let win = adw::Window::builder()
        .modal(true)
        .title(title)
        .default_width(480)
        .default_height(640)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let date_str = if let Some(dt) = act.start_datetime_local {
        dt.format("%-d %B %Y, %H:%M").to_string()
    } else {
        act.date.format("%-d %B %Y").to_string()
    };
    let date_label = gtk::Label::builder()
        .label(date_str)
        .css_classes(["caption", "dim-label"])
        .build();
    header.set_title_widget(Some(&date_label));

    let refresh_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh detailed activity data from Intervals.icu")
        .css_classes(["flat", "circular"])
        .build();
    header.pack_end(&refresh_btn);

    // ── Schedule This Ride button ─────────────────────────────────────────────
    let schedule_btn_icu = gtk::Button::builder()
        .icon_name("calendar-symbolic")
        .tooltip_text("Schedule this ride as a future workout for AI nutrition advice")
        .css_classes(["flat", "circular"])
        .build();
    let act_name = if act.name.is_empty() {
        if is_run(&act.sport_type) {
            "Run".to_string()
        } else {
            "Ride".to_string()
        }
    } else {
        act.name.clone()
    };
    let dur_sched = act.duration_secs;
    let avg_w_sched = act.average_watts;
    let pool_sched_icu = pool.clone();
    let rt_sched_icu = rt_handle.clone();
    schedule_btn_icu.connect_clicked(move |btn| {
        show_schedule_icu_ride_dialog(
            btn.upcast_ref(),
            &act_name,
            dur_sched.unwrap_or(0),
            avg_w_sched,
            ftp,
            pool_sched_icu.clone(),
            rt_sched_icu.clone(),
        );
    });
    header.pack_end(&schedule_btn_icu);

    toolbar_view.add_top_bar(&header);

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

    // ── Stats group ───────────────────────────────────────────────────────────
    let stats_group = adw::PreferencesGroup::builder()
        .title("Session Stats")
        .build();

    let make_row = |lbl: &str, val: String| {
        let row = adw::ActionRow::builder().title(lbl).build();
        let v = gtk::Label::builder()
            .label(&val)
            .css_classes(["dim-label", "numeric"])
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&v);
        row
    };

    if let Some(dur) = act.duration_secs {
        stats_group.add(&make_row(
            "Duration",
            WorkoutEngine::format_duration(dur).to_string(),
        ));
    }

    if is_run(&act.sport_type) {
        if let Some(dist) = act.distance_m {
            stats_group.add(&make_row("Distance", format_distance(dist)));
        }
        if let (Some(dist), Some(dur)) = (act.distance_m, act.duration_secs) {
            stats_group.add(&make_row("Avg Pace", format_average_pace(dist, dur)));
        }
        if let Some(cad) = act.average_cadence {
            stats_group.add(&make_row("Avg Cadence", format!("{:.0} spm", cad)));
        }
        if let Some(p) = act.average_watts {
            stats_group.add(&make_row("Avg Running Power", format!("{p} W")));
        }
    } else {
        match act.average_watts {
            Some(p) => stats_group.add(&make_row("Avg Power", format!("{p} W"))),
            None => stats_group.add(&make_row("Avg Power", "—".into())),
        }
        match act.normalized_watts {
            Some(p) => stats_group.add(&make_row("Normalised Power", format!("{p} W"))),
            None => stats_group.add(&make_row("Normalised Power", "—".into())),
        }
        if let (Some(np), Some(avg)) = (act.normalized_watts, act.average_watts) {
            if avg > 0 {
                stats_group.add(&make_row(
                    "Variability Index",
                    format!("{:.2}", np as f32 / avg as f32),
                ));
            }
        }
        if ftp > 0 {
            match act.normalized_watts {
                Some(np) => stats_group.add(&make_row(
                    "Intensity Factor",
                    format!("{:.2}", np as f32 / ftp as f32),
                )),
                None => stats_group.add(&make_row("Intensity Factor", "—".into())),
            }
        }
        if let (Some(avg_w), Some(dur)) = (act.average_watts, act.duration_secs) {
            let kj = avg_w as f32 * dur as f32 / 1000.0;
            stats_group.add(&make_row("Kilojoules", format!("{kj:.0} kJ")));
        }
        if weight_kg > 0.0 {
            if let Some(np) = act.normalized_watts {
                stats_group.add(&make_row(
                    "W/kg",
                    format!("{:.2} W/kg", np as f32 / weight_kg),
                ));
            }
        }
        if let Some(cad) = act.average_cadence {
            stats_group.add(&make_row("Avg Cadence", format!("{:.0} rpm", cad)));
        }
        if let Some(dist) = act.distance_m {
            stats_group.add(&make_row("Distance", format_distance(dist)));
        }
    }

    if let Some(t) = act.tss {
        stats_group.add(&make_row("TSS", format!("{}", t as u32)));
    }
    if let Some(hr) = act.average_hr {
        stats_group.add(&make_row("Avg Heart Rate", format!("{hr} bpm")));
    }
    if let Some(hr) = act.max_hr {
        stats_group.add(&make_row("Max Heart Rate", format!("{hr} bpm")));
    }
    if let (Some(np_or_avg), Some(hr)) =
        (act.normalized_watts.or(act.average_watts), act.average_hr)
    {
        if hr > 0 && !is_run(&act.sport_type) {
            stats_group.add(&make_row(
                "Aerobic Efficiency",
                format!("{:.2} W/bpm", np_or_avg as f32 / hr as f32),
            ));
        }
    }
    if let Some(elev) = act.elevation_gain_m.filter(|&e| e >= 1.0) {
        stats_group.add(&make_row("Elevation Gain", format!("{:.0} m", elev)));
    }

    inner.append(&stats_group);

    // ── Route map (Shumate tile map) ──────────────────────────────────────────
    let route_map = libshumate::SimpleMap::new();
    route_map.set_hexpand(true);
    route_map.set_size_request(-1, 220);
    route_map.set_map_source(Some(&libshumate::RasterRenderer::from_url(
        "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
    )));
    let current_path_layer: Rc<RefCell<Option<libshumate::PathLayer>>> =
        Rc::new(RefCell::new(None));
    let route_section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    route_section.append(
        &gtk::Label::builder()
            .label("Route")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );
    route_section.append(&route_map);
    inner.append(&route_section);

    // ── Elevation profile ─────────────────────────────────────────────────────
    let elev_area = gtk::DrawingArea::builder()
        .content_height(70)
        .hexpand(true)
        .build();
    let elev_section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    elev_section.append(
        &gtk::Label::builder()
            .label("Elevation Profile")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );
    elev_section.append(&elev_area);
    inner.append(&elev_section);

    // ── Performance chart ─────────────────────────────────────────────────────
    let perf_area = gtk::DrawingArea::builder()
        .content_height(90)
        .hexpand(true)
        .build();
    let perf_section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    perf_section.append(
        &gtk::Label::builder()
            .label("Heart Rate & Power / Pace")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );
    perf_section.append(&perf_area);
    let legend_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(18)
        .halign(gtk::Align::Center)
        .build();
    legend_row.append(
        &gtk::Label::builder()
            .label("— HR")
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    legend_row.append(
        &gtk::Label::builder()
            .label("— Power / Pace")
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    perf_section.append(&legend_row);
    inner.append(&perf_section);

    // ── Status row ────────────────────────────────────────────────────────────
    let status_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();
    let spinner = gtk::Spinner::builder().visible(false).build();
    let status_label = gtk::Label::builder()
        .css_classes(["dim-label", "caption"])
        .visible(false)
        .wrap(true)
        .halign(gtk::Align::Center)
        .build();
    status_box.append(&spinner);
    status_box.append(&status_label);
    inner.append(&status_box);

    inner.append(
        &gtk::Label::builder()
            .label("Data synced from Intervals.icu")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Center)
            .wrap(true)
            .build(),
    );

    // ── Streams data and draw wiring ──────────────────────────────────────────
    let streams_data: Rc<RefCell<Option<ActivityStreams>>> = Rc::new(RefCell::new(None));

    {
        let sd = Rc::clone(&streams_data);
        elev_area.set_draw_func(move |_w, cr, w, h| {
            if let Some(s) = sd.borrow().as_ref() {
                if s.has_altitude() {
                    let pairs = s.elevation_pairs();
                    let pts = ActivityStreams::downsample(&pairs, 500);
                    draw_elevation_profile(cr, &pts, w, h);
                }
            }
        });
    }
    {
        let sd = Rc::clone(&streams_data);
        perf_area.set_draw_func(move |_w, cr, w, h| {
            if let Some(s) = sd.borrow().as_ref() {
                let hr = ActivityStreams::downsample(&s.heartrate, 500);
                let perf: Vec<f32> = if !s.watts.is_empty() {
                    ActivityStreams::downsample(&s.watts, 500)
                        .into_iter()
                        .map(|v| v as f32)
                        .collect()
                } else {
                    ActivityStreams::downsample(&s.velocity_ms, 500)
                };
                draw_perf_chart(cr, &hr, &perf, w, h);
            }
        });
    }

    // Populate closure — called both from the initial DB cache check and from the refresh timer.
    let populate_streams: Rc<dyn Fn(ActivityStreams)> = {
        let streams_data = Rc::clone(&streams_data);
        let route_section = route_section.clone();
        let route_map = route_map.clone();
        let current_path_layer = Rc::clone(&current_path_layer);
        let elev_section = elev_section.clone();
        let elev_area = elev_area.clone();
        let perf_section = perf_section.clone();
        let perf_area = perf_area.clone();
        let status_label = status_label.clone();
        let spinner = spinner.clone();
        Rc::new(move |s: ActivityStreams| {
            let has_gps = s.has_gps();
            let has_alt = s.has_altitude();
            let has_perf = s.has_hr() || s.has_power() || s.has_velocity();

            if has_gps {
                let latlng = &s.latlng;
                let lat_min = latlng
                    .iter()
                    .map(|&(lat, _)| lat)
                    .fold(f64::INFINITY, f64::min);
                let lat_max = latlng
                    .iter()
                    .map(|&(lat, _)| lat)
                    .fold(f64::NEG_INFINITY, f64::max);
                let lng_min = latlng
                    .iter()
                    .map(|&(_, lng)| lng)
                    .fold(f64::INFINITY, f64::min);
                let lng_max = latlng
                    .iter()
                    .map(|&(_, lng)| lng)
                    .fold(f64::NEG_INFINITY, f64::max);
                let center_lat = (lat_min + lat_max) / 2.0;
                let center_lng = (lng_min + lng_max) / 2.0;
                let max_span = (lat_max - lat_min).max(lng_max - lng_min).max(1e-9);
                let zoom = ((360.0_f64 / max_span).log2() - 1.0).clamp(2.0, 16.0);

                if let Some(viewport) = route_map.viewport() {
                    viewport.set_location(center_lat, center_lng);
                    viewport.set_zoom_level(zoom);
                    if let Some(old) = current_path_layer.borrow().as_ref() {
                        route_map.remove_overlay_layer(old);
                    }
                    let path_layer = libshumate::PathLayer::new(&viewport);
                    let pts = ActivityStreams::downsample(latlng, 500);
                    for &(lat, lng) in &pts {
                        path_layer.add_node(&libshumate::Coordinate::new_full(lat, lng));
                    }
                    let stroke = gtk::gdk::RGBA::new(0.35, 0.60, 1.0, 0.9);
                    path_layer.set_stroke_color(Some(&stroke));
                    path_layer.set_stroke_width(3.0);
                    route_map.add_overlay_layer(&path_layer);
                    *current_path_layer.borrow_mut() = Some(path_layer);
                }
            }

            *streams_data.borrow_mut() = Some(s);
            route_section.set_visible(has_gps);
            elev_section.set_visible(has_alt);
            if has_alt {
                elev_area.queue_draw();
            }
            perf_section.set_visible(has_perf);
            if has_perf {
                perf_area.queue_draw();
            }
            spinner.set_spinning(false);
            spinner.set_visible(false);
            status_label.set_visible(false);
        })
    };

    // Shared fetch closure — called on first open (when not cached) and on Refresh click.
    let do_fetch: Rc<dyn Fn()> = {
        let pool = pool.clone();
        let rt = rt_handle.clone();
        let icu_id = icu_id.clone();
        let populate = Rc::clone(&populate_streams);
        let spinner = spinner.clone();
        let status_label = status_label.clone();
        Rc::new(move || {
            let api_key = match keystore::get_secret(keystore::KEY_INTERVALS_API).unwrap_or(None) {
                Some(s) if !s.is_empty() => s,
                _ => {
                    status_label
                        .set_label("Intervals.icu credentials not set — configure in Preferences");
                    status_label.set_visible(true);
                    return;
                }
            };

            spinner.set_spinning(true);
            spinner.set_visible(true);
            status_label.set_label("Loading detailed data…");
            status_label.set_visible(true);

            let (tx, rx) = async_channel::bounded::<anyhow::Result<String>>(1);
            rt.spawn({
                let api_key = api_key.clone();
                let icu_id = icu_id.clone();
                let pool = pool.clone();
                async move {
                    // The athlete ID is read here, not on the GTK thread: a
                    // block_on against SQLite stalls the GLib loop whenever the
                    // database is busy (CLAUDE.md §2.3).
                    let athlete_id = match settings::load_intervals(&pool).await {
                        Ok(s) if !s.athlete_id.is_empty() => s.athlete_id,
                        Ok(_) => {
                            tx.send(Err(anyhow::anyhow!(
                                "Intervals.icu credentials not set — configure in Preferences"
                            )))
                            .await
                            .ok();
                            return;
                        }
                        Err(e) => {
                            tx.send(Err(anyhow::anyhow!(
                                "Could not read your Intervals.icu settings: {e}"
                            )))
                            .await
                            .ok();
                            return;
                        }
                    };
                    let r = crate::ai::intervals::fetch_combined_activity_data(
                        &athlete_id,
                        &api_key,
                        &icu_id,
                    )
                    .await;
                    tx.send(r).await.ok();
                }
            });

            let pool = pool.clone();
            let rt = rt.clone();
            let icu_id = icu_id.clone();
            let populate = Rc::clone(&populate);
            let spinner = spinner.clone();
            let status_label = status_label.clone();
            glib::timeout_add_local(Duration::from_millis(200), move || match rx.try_recv() {
                Ok(Ok(json)) => {
                    tracing::debug!("Intervals.icu streams loaded for {icu_id}");
                    // Cache the streams in the background — this is a write of a
                    // potentially large blob and must not block the GLib loop.
                    rt.spawn({
                        let pool = pool.clone();
                        let icu_id = icu_id.clone();
                        let json = json.clone();
                        async move {
                            if let Err(e) = db::save_activity_streams(&pool, &icu_id, &json).await {
                                tracing::warn!("Could not cache activity streams: {e}");
                            }
                        }
                    });
                    match ActivityStreams::from_json(&json) {
                        Some(s) => {
                            tracing::info!(
                                gps_pts = s.latlng.len(),
                                altitude_pts = s.altitude_m.len(),
                                hr_pts = s.heartrate.len(),
                                watts_pts = s.watts.len(),
                                "Intervals.icu streams parsed"
                            );
                            populate(s);
                        }
                        None => {
                            spinner.set_spinning(false);
                            spinner.set_visible(false);
                            status_label.set_label("Activity has no detailed stream data");
                            status_label.set_visible(true);
                        }
                    }
                    glib::ControlFlow::Break
                }
                Ok(Err(e)) => {
                    tracing::warn!("Intervals.icu streams fetch failed: {e}");
                    spinner.set_spinning(false);
                    spinner.set_visible(false);
                    status_label.set_label(&format!("Failed to load: {e}"));
                    status_label.set_visible(true);
                    glib::ControlFlow::Break
                }
                Err(async_channel::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(_) => {
                    spinner.set_spinning(false);
                    spinner.set_visible(false);
                    glib::ControlFlow::Break
                }
            });
        })
    };

    // Initial cache check. This used to be a block_on, justified as a "fast
    // synchronous read" — but it reads a whole stream blob, and a ride
    // checkpoint every 30 s can have the database busy when it runs. The dialog
    // opens straight away and fills in when the read lands (CLAUDE.md §2.3).
    {
        let populate = Rc::clone(&populate_streams);
        let do_fetch = Rc::clone(&do_fetch);
        let pool_cache = pool.clone();
        let icu_id_cache = icu_id.clone();
        crate::ui::spawn_to_main(
            rt_handle,
            async move { db::get_activity_streams(&pool_cache, &icu_id_cache).await },
            move |result| {
                let cached = match result {
                    Ok(c) => c,
                    Err(e) => {
                        // Treat an unreadable cache as no cache: fetching again
                        // is the same recovery either way.
                        tracing::warn!("Could not read the cached activity streams: {e}");
                        None
                    }
                };
                match cached.as_deref().and_then(ActivityStreams::from_json) {
                    Some(s) => {
                        let needs_gps_refresh = !s.has_gps();
                        populate(s);
                        if needs_gps_refresh {
                            // Cache predates GPS support, or the streams endpoint
                            // 404'd and was never retried against the map endpoint.
                            do_fetch();
                        }
                    }
                    // Nothing cached, or the cache will not parse — fetch.
                    None => do_fetch(),
                }
            },
        );
    }

    // Refresh button: force a re-fetch even if data is already displayed.
    refresh_btn.connect_clicked({
        let do_fetch = Rc::clone(&do_fetch);
        move |_| do_fetch()
    });

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

#[allow(clippy::too_many_arguments)]
pub fn show_session_detail(
    session: &crate::data::session::Session,
    title: &str,
    local_dt: chrono::DateTime<Local>,
    ftp: u32,
    weight_kg: f32,
    workout: Option<&Workout>,
    parent: Option<&gtk::Window>,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    reload_holder: ReloadHolder,
) {
    let win = adw::Window::builder()
        .modal(true)
        .title(title)
        .default_width(440)
        .default_height(560)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let date_label = gtk::Label::builder()
        .label(local_dt.format("%-d %B %Y, %H:%M").to_string())
        .css_classes(["caption", "dim-label"])
        .build();
    header.set_title_widget(Some(&date_label));

    // ── Schedule This Ride button ─────────────────────────────────────────────
    let schedule_btn = gtk::Button::builder()
        .icon_name("calendar-symbolic")
        .tooltip_text("Schedule this ride as a future workout for AI nutrition advice")
        .css_classes(["flat", "circular"])
        .build();
    let session_for_sched = session.clone();
    let title_sched = title.to_string();
    let pool_sched = pool.clone();
    let rt_sched = rt_handle.clone();
    schedule_btn.connect_clicked(move |btn| {
        show_schedule_past_ride_dialog(
            btn.upcast_ref(),
            &session_for_sched,
            &title_sched,
            ftp,
            pool_sched.clone(),
            rt_sched.clone(),
        );
    });
    header.pack_end(&schedule_btn);

    // ── Export FIT button ─────────────────────────────────────────────────────
    // Saves wherever the rider chooses, so the file can go straight into Strava,
    // Garmin Connect or a shared folder.
    let export_btn = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export this ride as a FIT file")
        .css_classes(["flat", "circular"])
        .build();
    let session_for_export = session.clone();
    let title_export = title.to_string();
    let pool_export = pool.clone();
    let rt_export = rt_handle.clone();
    export_btn.connect_clicked(move |btn| {
        let parent = btn.root().and_downcast::<gtk::Window>();
        let session_save = session_for_export.clone();
        let filename = crate::data::fit::suggested_filename(&session_for_export, &title_export);
        let pool_a = pool_export.clone();

        // The profile is read before the file chooser opens, off the GTK thread.
        // It carries the FTP and heart-rate limits the training-load figure in
        // the file is scaled to — and Garmin reads that figure from the file
        // rather than recomputing it, so exporting on a default profile would
        // publish a wrong number as fact. A failed read cancels the export.
        crate::ui::spawn_to_main(
            &rt_export,
            async move { db::load_or_create_athlete(&pool_a).await },
            move |result| {
                let athlete = match result {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::error!("Could not read your profile for export: {e}");
                        let alert = adw::AlertDialog::builder()
                            .heading("Could not export")
                            .body(
                                "Your athlete profile could not be read. The file was \
                                 not written, because its training-load figure is \
                                 scaled to your FTP and heart-rate limits.",
                            )
                            .build();
                        alert.add_response("ok", "_OK");
                        alert.set_default_response(Some("ok"));
                        if let Some(p) = parent.as_ref() {
                            alert.present(Some(p));
                        }
                        return;
                    }
                };

                let dialog = gtk::FileDialog::builder()
                    .title("Export FIT File")
                    .accept_label("Export")
                    .initial_name(filename)
                    .build();
                dialog.save(
                    parent.as_ref(),
                    None::<&gio::Cancellable>,
                    move |result| match result {
                        Ok(file) => {
                            let Some(path) = file.path() else {
                                tracing::error!("Export target has no local path");
                                return;
                            };
                            match crate::data::fit::write_session_fit(
                                &path,
                                &session_save,
                                &athlete,
                            ) {
                                Ok(()) => tracing::info!("Exported FIT to {}", path.display()),
                                Err(e) => tracing::error!("FIT export failed: {e}"),
                            }
                        }
                        // The rider dismissing the file chooser is not an error.
                        Err(e) if e.matches(gtk::DialogError::Dismissed) => {}
                        Err(e) => tracing::error!("Export dialog failed: {e}"),
                    },
                );
            },
        );
    });
    header.pack_end(&export_btn);

    toolbar_view.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // ── Name ──────────────────────────────────────────────────────────────
    // Saved on focus-out and on Enter; clearing the field restores the default
    // name (the workout's, or "Unstructured Ride").
    let name_group = adw::PreferencesGroup::builder().title("Activity").build();
    let name_row = adw::EntryRow::builder().title("Name").build();
    name_row.set_text(title);
    name_group.add(&name_row);
    inner.append(&name_group);

    // ── Intervals.icu link ────────────────────────────────────────────────
    // Shown only when this ride was matched to an Intervals.icu activity, so a
    // ride that quietly vanished from the calendar is explainable — and undoable.
    if let Some(icu_id) = session.icu_id.clone() {
        let link_row = adw::ActionRow::builder()
            .title("Synced with Intervals.icu")
            .subtitle("Shown once — the copy from Intervals.icu is hidden")
            .build();
        link_row.add_prefix(&gtk::Image::from_icon_name("emblem-synchronizing-symbolic"));

        let unlink_btn = gtk::Button::builder()
            .label("Unlink")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("Treat these as two separate rides and show both")
            .build();
        link_row.add_suffix(&unlink_btn);
        name_group.add(&link_row);

        let session_id = session.id;
        let pool_unlink = pool.clone();
        let rt_unlink = rt_handle.clone();
        let reload_unlink = Rc::clone(&reload_holder);
        unlink_btn.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            let pool = pool_unlink.clone();
            let reload = Rc::clone(&reload_unlink);
            let row = link_row.clone();
            tracing::info!("Unlinking session {session_id} from Intervals.icu {icu_id}");
            crate::ui::spawn_to_main(
                &rt_unlink,
                async move { db::unlink_session_from_icu(&pool, session_id).await },
                move |result| match result {
                    Ok(()) => {
                        row.set_subtitle("Unlinked — both rides will be shown");
                        if let Some(cb) = reload.borrow().as_ref() {
                            cb();
                        }
                    }
                    Err(e) => tracing::error!("unlink session failed: {e}"),
                },
            );
        });
    }

    {
        let session_id = session.id;
        let pool_name = pool.clone();
        let rt_name = rt_handle.clone();
        let reload_name = Rc::clone(&reload_holder);
        let save_name = move |text: String| {
            let pool = pool_name.clone();
            let reload = Rc::clone(&reload_name);
            crate::ui::spawn_to_main(
                &rt_name,
                async move { db::set_session_title(&pool, session_id, &text).await },
                move |result| match result {
                    Ok(()) => {
                        if let Some(cb) = reload.borrow().as_ref() {
                            cb();
                        }
                    }
                    Err(e) => tracing::error!("rename session failed: {e}"),
                },
            );
        };
        let save_on_apply = save_name.clone();
        name_row.connect_apply(move |row| save_on_apply(row.text().to_string()));
        let focus_controller = gtk::EventControllerFocus::new();
        let row_for_focus = name_row.clone();
        focus_controller.connect_leave(move |_| save_name(row_for_focus.text().to_string()));
        name_row.add_controller(focus_controller);
    }

    // ── Stats group ───────────────────────────────────────────────────────
    let stats_group = adw::PreferencesGroup::builder()
        .title("Session Stats")
        .build();

    let make_row = |lbl: &str, val: String| {
        let row = adw::ActionRow::builder().title(lbl).build();
        let v = gtk::Label::builder()
            .label(&val)
            .css_classes(["dim-label", "numeric"])
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&v);
        row
    };

    let avg_power = session.average_power();
    let np = session.normalised_power();

    let dur = session.duration_secs() as u32;
    stats_group.add(&make_row(
        "Duration",
        WorkoutEngine::format_duration(dur).to_string(),
    ));

    match avg_power {
        Some(p) => stats_group.add(&make_row("Avg Power", format!("{} W", p as u32))),
        None => stats_group.add(&make_row("Avg Power", "—".into())),
    }

    let max_power: Option<u32> = session
        .data_points
        .iter()
        .filter_map(|p| p.power_watts)
        .max();
    if let Some(mp) = max_power {
        stats_group.add(&make_row("Max Power", format!("{} W", mp)));
    }

    match np {
        Some(p) => stats_group.add(&make_row("Normalised Power", format!("{} W", p as u32))),
        None => stats_group.add(&make_row("Normalised Power", "—".into())),
    }

    // Variability Index = NP / Avg Power — measures pacing steadiness
    if let (Some(np_val), Some(avg_val)) = (np, avg_power) {
        if avg_val > 0.0 {
            stats_group.add(&make_row(
                "Variability Index",
                format!("{:.2}", np_val / avg_val),
            ));
        }
    }

    if ftp > 0 {
        match np {
            Some(p) => stats_group.add(&make_row(
                "Intensity Factor",
                format!("{:.2}", p / ftp as f32),
            )),
            None => stats_group.add(&make_row("Intensity Factor", "—".into())),
        }
        match session.tss(ftp) {
            Some(t) => stats_group.add(&make_row("TSS", format!("{}", t as u32))),
            None => stats_group.add(&make_row("TSS", "—".into())),
        }
    }

    // W/kg using Normalised Power
    if weight_kg > 0.0 {
        if let Some(np_val) = np {
            stats_group.add(&make_row("W/kg", format!("{:.2} W/kg", np_val / weight_kg)));
        }
    }

    stats_group.add(&make_row(
        "Kilojoules",
        format!("{:.0} kJ", session.kilojoules()),
    ));

    // Heart rate — avg, max, and aerobic efficiency
    let hr_readings: Vec<u32> = session
        .data_points
        .iter()
        .filter_map(|p| p.heart_rate_bpm)
        .collect();
    if !hr_readings.is_empty() {
        let avg_hr = hr_readings.iter().sum::<u32>() as f32 / hr_readings.len() as f32;
        let max_hr = *hr_readings.iter().max().expect("non-empty");
        stats_group.add(&make_row(
            "Avg Heart Rate",
            format!("{} bpm", avg_hr as u32),
        ));
        stats_group.add(&make_row("Max Heart Rate", format!("{} bpm", max_hr)));
        // EF = NP / Avg HR — rising EF over time = improving aerobic fitness
        if let Some(np_val) = np {
            if avg_hr > 0.0 {
                stats_group.add(&make_row(
                    "Aerobic Efficiency",
                    format!("{:.2} W/bpm", np_val / avg_hr),
                ));
            }
        }
    }

    // Cadence (excluding zeros = coasting)
    let cad_readings: Vec<u32> = session
        .data_points
        .iter()
        .filter_map(|p| p.cadence_rpm)
        .filter(|&c| c > 0)
        .collect();
    if !cad_readings.is_empty() {
        let avg_cad = cad_readings.iter().sum::<u32>() as f32 / cad_readings.len() as f32;
        stats_group.add(&make_row("Avg Cadence", format!("{} rpm", avg_cad as u32)));
    }

    inner.append(&stats_group);

    // ── Zone breakdown ────────────────────────────────────────────────────
    let mut zone_secs = [0u32; 7];
    for dp in &session.data_points {
        if let Some(watts) = dp.power_watts {
            zone_secs[power_zone_index(watts, ftp)] += 1;
        }
    }
    let has_power = zone_secs.iter().any(|&s| s > 0);

    if has_power {
        let zone_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        zone_section.append(
            &gtk::Label::builder()
                .label("Time in Zone")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        let zone_bar =
            ZoneBar::new("Power zone distribution bar: proportional time in zones Z1 through Z7");
        zone_bar.set_seconds(&zone_secs);
        zone_section.append(zone_bar.widget());

        let zone_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        let total: u32 = zone_secs.iter().sum();
        for (i, label) in ["Z1", "Z2", "Z3", "Z4", "Z5", "Z6", "Z7"]
            .iter()
            .enumerate()
        {
            let pct = (zone_secs[i] * 100).checked_div(total).unwrap_or(0);
            let text = if pct > 0 {
                format!("{} {}%", label, pct)
            } else {
                (*label).to_string()
            };
            zone_legend.append(
                &gtk::Label::builder()
                    .label(&text)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }
        zone_section.append(&zone_legend);
        inner.append(&zone_section);
    }

    // ── Interval Analysis ─────────────────────────────────────────────────
    if let Some(wk) = workout {
        let stats = session.interval_analysis(&wk.segments, ftp);
        let active_stats: Vec<_> = stats.iter().filter(|s| s.is_active).collect();

        if !active_stats.is_empty() {
            let interval_group = adw::PreferencesGroup::builder()
                .title("Interval Analysis")
                .build();

            for s in &active_stats {
                let row = adw::ActionRow::builder()
                    .title(&s.label)
                    .subtitle(format!("Target: {} W", s.target_watts))
                    .build();
                let avg_str = match s.avg_watts {
                    Some(a) => format!("{} W", a as u32),
                    None => "—".into(),
                };
                let avg_lbl = gtk::Label::builder()
                    .label(&avg_str)
                    .css_classes(["numeric", "caption"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Measured average power for this segment")
                    .build();
                let pct = (s.seconds_on_target * 100)
                    .checked_div(s.duration_secs)
                    .unwrap_or(0);
                let pct_lbl = gtk::Label::builder()
                    .label(format!("{pct}%"))
                    .css_classes(["numeric", "caption"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Seconds within ±10% of target")
                    .build();
                if pct >= 80 {
                    pct_lbl.add_css_class("success");
                } else if pct >= 60 {
                    pct_lbl.add_css_class("warning");
                } else if s.avg_watts.is_some() {
                    pct_lbl.add_css_class("error");
                }
                row.add_suffix(&avg_lbl);
                let sep = gtk::Separator::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                row.add_suffix(&sep);
                row.add_suffix(&pct_lbl);
                interval_group.add(&row);
            }

            if let Some(overall) = session.compliance_pct(&wk.segments, ftp) {
                let summary_row = adw::ActionRow::builder()
                    .title("Overall Compliance")
                    .build();
                let overall_lbl = gtk::Label::builder()
                    .label(format!("{overall}%"))
                    .css_classes(["numeric", "heading"])
                    .valign(gtk::Align::Center)
                    .build();
                if overall >= 80 {
                    overall_lbl.add_css_class("success");
                } else if overall >= 60 {
                    overall_lbl.add_css_class("warning");
                } else {
                    overall_lbl.add_css_class("error");
                }
                summary_row.add_suffix(&overall_lbl);
                interval_group.add(&summary_row);
            }

            inner.append(&interval_group);
        }

        // ── FTP Test Suggestion ────────────────────────────────────────────
        let name_lower = wk.name.to_lowercase();
        if name_lower.contains("ramp test") {
            if let Some(peak_1min) = session.peak_power_for_duration(60) {
                let suggested_ftp = (peak_1min as f32 * 0.75) as u32;
                let ftp_group = adw::PreferencesGroup::builder()
                    .title("FTP Suggestion")
                    .description("Based on your peak 1-minute power from this ramp test")
                    .build();
                let ftp_row = adw::ActionRow::builder()
                    .title("Suggested FTP")
                    .subtitle("Peak 1-min power × 0.75 — update in Preferences → Athlete Profile")
                    .build();
                ftp_row.add_suffix(
                    &gtk::Label::builder()
                        .label(format!("{suggested_ftp} W"))
                        .css_classes(["numeric", "title-3", "accent"])
                        .valign(gtk::Align::Center)
                        .build(),
                );
                ftp_group.add(&ftp_row);
                inner.append(&ftp_group);
            }
        } else if name_lower.contains("20-minute ftp test") || name_lower.contains("20 minute ftp")
        {
            // The 20-min effort segment is at elapsed 20..40 min (after 10 wu + 5 pre-load + 5 rv)
            if let Some(peak_20min) = session.peak_power_for_duration(20 * 60) {
                let suggested_ftp = (peak_20min as f32 * 0.95) as u32;
                let ftp_group = adw::PreferencesGroup::builder()
                    .title("FTP Suggestion")
                    .description("Based on your peak 20-minute power from this test")
                    .build();
                let ftp_row = adw::ActionRow::builder()
                    .title("Suggested FTP")
                    .subtitle("Peak 20-min power × 0.95 — update in Preferences → Athlete Profile")
                    .build();
                ftp_row.add_suffix(
                    &gtk::Label::builder()
                        .label(format!("{suggested_ftp} W"))
                        .css_classes(["numeric", "title-3", "accent"])
                        .valign(gtk::Align::Center)
                        .build(),
                );
                ftp_group.add(&ftp_row);
                inner.append(&ftp_group);
            }
        }
    }

    // ── Route map (GPS activities, Shumate tile map) ──────────────────────
    let gps_pts: Vec<(f64, f64)> = session
        .data_points
        .iter()
        .filter_map(|dp| match (dp.lat, dp.lng) {
            (Some(lat), Some(lng)) => Some((lat, lng)),
            _ => None,
        })
        .collect();

    if gps_pts.len() >= 2 {
        let route_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        route_section.append(
            &gtk::Label::builder()
                .label("Route")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );
        let route_map = libshumate::SimpleMap::new();
        route_map.set_hexpand(true);
        route_map.set_size_request(-1, 220);
        route_map.set_map_source(Some(&libshumate::RasterRenderer::from_url(
            "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
        )));

        let lat_min = gps_pts
            .iter()
            .map(|&(lat, _)| lat)
            .fold(f64::INFINITY, f64::min);
        let lat_max = gps_pts
            .iter()
            .map(|&(lat, _)| lat)
            .fold(f64::NEG_INFINITY, f64::max);
        let lng_min = gps_pts
            .iter()
            .map(|&(_, lng)| lng)
            .fold(f64::INFINITY, f64::min);
        let lng_max = gps_pts
            .iter()
            .map(|&(_, lng)| lng)
            .fold(f64::NEG_INFINITY, f64::max);
        let center_lat = (lat_min + lat_max) / 2.0;
        let center_lng = (lng_min + lng_max) / 2.0;
        let max_span = (lat_max - lat_min).max(lng_max - lng_min).max(1e-9);
        let zoom = ((360.0_f64 / max_span).log2() - 1.0).clamp(2.0, 16.0);

        if let Some(viewport) = route_map.viewport() {
            viewport.set_location(center_lat, center_lng);
            viewport.set_zoom_level(zoom);
            let path_layer = libshumate::PathLayer::new(&viewport);
            let pts = ActivityStreams::downsample(&gps_pts, 500);
            for &(lat, lng) in &pts {
                path_layer.add_node(&libshumate::Coordinate::new_full(lat, lng));
            }
            let stroke = gtk::gdk::RGBA::new(0.35, 0.60, 1.0, 0.9);
            path_layer.set_stroke_color(Some(&stroke));
            path_layer.set_stroke_width(3.0);
            route_map.add_overlay_layer(&path_layer);
        }

        route_section.append(&route_map);
        inner.append(&route_section);
    }

    // ── Self evaluation ───────────────────────────────────────────────────
    let eval_group = adw::PreferencesGroup::builder()
        .title("Self Evaluation")
        .build();

    if let Some(rpe) = session.rpe {
        let rpe_label = match rpe {
            1 => "Very Easy",
            2 => "Easy",
            3 => "Moderate",
            4 => "Hard",
            5 => "Very Hard",
            _ => "Maximum Effort",
        };

        let row = adw::ActionRow::builder()
            .title(rpe_label)
            .subtitle(format!("RPE {rpe}/6"))
            .build();

        if let Some(texture) = crate::ui::resources::rpe_texture(rpe) {
            let image = gtk::Image::builder()
                .paintable(&texture)
                .pixel_size(48)
                .valign(gtk::Align::Center)
                .build();
            row.add_prefix(&image);
        }

        eval_group.add(&row);
    } else {
        let session_id_eval = session.id;
        let pool_eval = pool.clone();
        let rt_eval = rt_handle.clone();
        let rh_eval = Rc::clone(&reload_holder);

        let rate_row = adw::ActionRow::builder()
            .title("No self-evaluation yet")
            .subtitle("Rate how hard this session felt")
            .activatable(true)
            .build();
        let rate_icon = gtk::Image::builder()
            .icon_name("starred-symbolic")
            .valign(gtk::Align::Center)
            .build();
        rate_row.add_prefix(&rate_icon);

        // Weak: rate_row is inside this window, so a strong capture would be a
        // cycle and the window — with the ride's streams — would never be freed.
        rate_row.connect_activated(glib::clone!(
            #[weak]
            win,
            move |row| {
                let pool = pool_eval.clone();
                let rt = rt_eval.clone();
                let rh = Rc::clone(&rh_eval);
                crate::ui::widgets::rpe_dialog::show(row, move |rpe| {
                    let pool = pool.clone();
                    let rh = Rc::clone(&rh);
                    let win = win.clone();
                    crate::ui::spawn_to_main(
                        &rt,
                        async move { db::save_session_rpe(&pool, session_id_eval, rpe).await },
                        move |res| {
                            if let Err(e) = res {
                                tracing::error!("save_session_rpe failed: {e}");
                            }
                            win.close();
                            if let Some(reload) = rh.borrow().as_ref() {
                                reload();
                            }
                        },
                    );
                });
            }
        ));

        eval_group.add(&rate_row);
    }

    inner.append(&eval_group);

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

// ── Cairo draw helpers ────────────────────────────────────────────────────────

fn draw_elevation_profile(cr: &cairo::Context, pts: &[(f32, f32)], width: i32, height: i32) {
    if pts.len() < 2 {
        return;
    }
    let x_max = pts.last().map(|&(x, _)| x).unwrap_or(1.0) as f64;
    let y_min = pts.iter().map(|&(_, y)| y).fold(f32::INFINITY, f32::min) as f64;
    let y_max = pts
        .iter()
        .map(|&(_, y)| y)
        .fold(f32::NEG_INFINITY, f32::max) as f64;
    let y_span = (y_max - y_min).max(1.0);

    let w = width as f64;
    let h = height as f64;
    let pad_t = 4.0;
    let pad_b = 2.0;
    let usable_h = h - pad_t - pad_b;

    let to_xy = |x: f32, y: f32| -> (f64, f64) {
        let sx = x as f64 / x_max * w;
        let sy = pad_t + (1.0 - (y as f64 - y_min) / y_span) * usable_h;
        (sx, sy)
    };

    let (sx0, sy0) = to_xy(pts[0].0, pts[0].1);

    // Amber filled area
    cr.set_source_rgba(1.0, 0.75, 0.20, 0.30);
    cr.move_to(sx0, h);
    cr.line_to(sx0, sy0);
    for &(x, y) in &pts[1..] {
        let (sx, sy) = to_xy(x, y);
        cr.line_to(sx, sy);
    }
    let (sx_last, _) = to_xy(pts[pts.len() - 1].0, pts[pts.len() - 1].1);
    cr.line_to(sx_last, h);
    cr.close_path();
    cr.fill().ok();

    // Amber outline
    cr.set_source_rgba(1.0, 0.75, 0.20, 0.85);
    cr.set_line_width(1.5);
    cr.move_to(sx0, sy0);
    for &(x, y) in &pts[1..] {
        let (sx, sy) = to_xy(x, y);
        cr.line_to(sx, sy);
    }
    cr.stroke().ok();
}

fn draw_perf_chart(cr: &cairo::Context, hr: &[u32], perf: &[f32], width: i32, height: i32) {
    let w = width as f64;
    let h = height as f64;
    let pad = 4.0;
    let usable_h = h - 2.0 * pad;

    // HR — warm red
    if hr.len() >= 2 {
        let hr_min = *hr.iter().min().unwrap_or(&0) as f64;
        let hr_max = *hr.iter().max().unwrap_or(&1) as f64;
        let hr_span = (hr_max - hr_min).max(1.0);
        let n = hr.len();

        cr.set_source_rgba(0.90, 0.30, 0.20, 0.75);
        cr.set_line_width(1.5);
        let y0 = pad + (1.0 - (hr[0] as f64 - hr_min) / hr_span) * usable_h;
        cr.move_to(0.0, y0);
        for (i, &v) in hr.iter().enumerate().skip(1) {
            let x = i as f64 / (n - 1) as f64 * w;
            let y = pad + (1.0 - (v as f64 - hr_min) / hr_span) * usable_h;
            cr.line_to(x, y);
        }
        cr.stroke().ok();
    }

    // Power or velocity — accent blue
    if perf.len() >= 2 {
        let perf_min = perf.iter().cloned().fold(f32::INFINITY, f32::min) as f64;
        let perf_max = perf.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as f64;
        let perf_span = (perf_max - perf_min).max(1.0);
        let n = perf.len();

        cr.set_source_rgba(0.35, 0.60, 1.0, 0.75);
        cr.set_line_width(1.5);
        let y0 = pad + (1.0 - (perf[0] as f64 - perf_min) / perf_span) * usable_h;
        cr.move_to(0.0, y0);
        for (i, &v) in perf.iter().enumerate().skip(1) {
            let x = i as f64 / (n - 1) as f64 * w;
            let y = pad + (1.0 - (v as f64 - perf_min) / perf_span) * usable_h;
            cr.line_to(x, y);
        }
        cr.stroke().ok();
    }
}

/// Show a date picker to schedule a local session as a future workout.
fn show_schedule_past_ride_dialog(
    parent: &gtk::Widget,
    session: &crate::data::session::Session,
    name: &str,
    ftp: u32,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Schedule This Ride")
        .build();
    dialog.set_body(&format!(
        "Pick a future date to add \"{}\" to your calendar so the AI coach can give nutrition advice.",
        name
    ));
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("schedule", "_Schedule");
    dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("schedule"));
    dialog.set_close_response("cancel");

    let calendar = gtk::Calendar::new();
    dialog.set_extra_child(Some(&calendar));

    let session = session.clone();
    let name = name.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "schedule" {
            return;
        }
        let dt = calendar.date();
        let date_str = format!(
            "{:04}-{:02}-{:02}",
            dt.year(),
            dt.month(),
            dt.day_of_month()
        );
        let session = session.clone();
        let name = name.clone();
        let pool = pool.clone();
        rt_handle.spawn(async move {
            match db::create_workout_from_session(&pool, &session, &name, ftp).await {
                Ok(workout_id) => match db::schedule_workout(&pool, workout_id, &date_str).await {
                    Ok(_) => tracing::info!("Scheduled past ride '{}' for {}", name, date_str),
                    Err(e) => tracing::error!("schedule_workout failed: {e}"),
                },
                Err(e) => tracing::error!("create_workout_from_session failed: {e}"),
            }
        });
    });

    dialog.present(Some(parent));
}

/// Show a date picker to schedule an Intervals.icu activity as a future workout.
fn show_schedule_icu_ride_dialog(
    parent: &gtk::Widget,
    name: &str,
    duration_secs: u32,
    avg_watts: Option<u32>,
    ftp: u32,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Schedule This Ride")
        .build();
    dialog.set_body(&format!(
        "Pick a future date to add \"{}\" to your calendar so the AI coach can give nutrition advice.",
        name
    ));
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("schedule", "_Schedule");
    dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("schedule"));
    dialog.set_close_response("cancel");

    let calendar = gtk::Calendar::new();
    dialog.set_extra_child(Some(&calendar));

    let name = name.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "schedule" {
            return;
        }
        let dt = calendar.date();
        let date_str = format!(
            "{:04}-{:02}-{:02}",
            dt.year(),
            dt.month(),
            dt.day_of_month()
        );
        let name = name.clone();
        let pool = pool.clone();
        rt_handle.spawn(async move {
            match db::create_workout_from_icu_activity(&pool, &name, duration_secs, avg_watts, ftp)
                .await
            {
                Ok(workout_id) => match db::schedule_workout(&pool, workout_id, &date_str).await {
                    Ok(_) => tracing::info!("Scheduled ICU ride '{}' for {}", name, date_str),
                    Err(e) => tracing::error!("schedule_workout failed: {e}"),
                },
                Err(e) => tracing::error!("create_workout_from_icu_activity failed: {e}"),
            }
        });
    });

    dialog.present(Some(parent));
}
