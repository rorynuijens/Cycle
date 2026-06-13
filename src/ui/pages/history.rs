use adw::prelude::*;
use chrono::{Datelike, Local, NaiveDate, Timelike};
use gtk::gio;
use libshumate::prelude::LocationExt;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use glib;
use sqlx::SqlitePool;

use crate::data::db::{self, SessionRecord};
use crate::data::keystore;
use crate::data::streams::ActivityStreams;
use crate::data::workout::Workout;
use crate::training::engine::WorkoutEngine;

pub type ReloadHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
type ToastFn = Rc<dyn Fn(adw::Toast)>;

fn zone_color(i: usize) -> (f64, f64, f64) {
    use crate::data::athlete::PowerZone;
    match i {
        0 => PowerZone::ActiveRecovery.rgb(),
        1 => PowerZone::Endurance.rgb(),
        2 => PowerZone::Tempo.rgb(),
        3 => PowerZone::Threshold.rgb(),
        4 => PowerZone::Vo2Max.rgb(),
        5 => PowerZone::Anaerobic.rgb(),
        _ => PowerZone::Neuromuscular.rgb(),
    }
}

fn zone_index(watts: u32, ftp: u32) -> usize {
    if ftp == 0 {
        return 0;
    }
    let pct = (watts as f64 / ftp as f64) * 100.0;
    match pct as u32 {
        0..=55 => 0,
        56..=75 => 1,
        76..=90 => 2,
        91..=105 => 3,
        106..=120 => 4,
        121..=150 => 5,
        _ => 6,
    }
}

fn is_run(sport_type: &str) -> bool {
    matches!(
        sport_type.to_lowercase().as_str(),
        "run" | "virtualrun" | "trailrun" | "snowshoe" | "ultrawalkrun"
    )
}

fn format_distance(distance_m: f32) -> String {
    if distance_m >= 1000.0 {
        format!("{:.2} km", distance_m / 1000.0)
    } else {
        format!("{:.0} m", distance_m)
    }
}

fn format_pace(distance_m: f32, duration_secs: u32) -> String {
    if distance_m < 1.0 || duration_secs == 0 {
        return "—".to_string();
    }
    let pace_secs_per_km = (duration_secs as f32 / (distance_m / 1000.0)) as u32;
    format!("{}:{:02}/km", pace_secs_per_km / 60, pace_secs_per_km % 60)
}

fn sport_badge(sport_type: &str) -> gtk::Label {
    let lower = sport_type.to_lowercase();
    let (label, css) = if is_run(&lower) {
        ("Run", "accent")
    } else if lower.contains("swim") {
        ("Swim", "accent")
    } else if matches!(lower.as_str(), "walk" | "hike") {
        ("Walk", "dim-label")
    } else if matches!(
        lower.as_str(),
        "weighttraining" | "strength" | "weights" | "muscleup"
    ) {
        ("Strength", "warning")
    } else if matches!(
        lower.as_str(),
        "yoga" | "pilates" | "flexibility" | "stretching"
    ) {
        ("Yoga", "dim-label")
    } else if matches!(
        lower.as_str(),
        "hiit" | "crossfit" | "highintensityintervaltraining" | "workout"
    ) {
        ("HIIT", "warning")
    } else if matches!(
        lower.as_str(),
        "rowing" | "virtualrowing" | "canoeing" | "kayaking"
    ) {
        ("Row", "accent")
    } else if matches!(
        lower.as_str(),
        "alpineski" | "backcountryski" | "nordicski" | "snowboard" | "rollerski"
    ) {
        ("Ski", "dim-label")
    } else if matches!(
        lower.as_str(),
        "elliptical" | "stairstepper" | "stairmaster"
    ) {
        ("Cardio", "warning")
    } else if matches!(lower.as_str(), "inlineskate" | "iceskate") {
        ("Skate", "dim-label")
    } else if matches!(lower.as_str(), "rockclimbing" | "climbing") {
        ("Climb", "dim-label")
    } else if matches!(lower.as_str(), "golf") {
        ("Golf", "dim-label")
    } else if lower.contains("ride")
        || lower.contains("cycling")
        || lower.contains("cycle")
        || lower.contains("virtual")
    {
        ("Ride", "success")
    } else {
        ("Other", "dim-label")
    };
    gtk::Label::builder()
        .label(label)
        .css_classes(["caption", "pill", css])
        .valign(gtk::Align::Center)
        .build()
}

fn activity_summary_text(duration_secs: u32, tss: Option<f32>) -> &'static str {
    let mins = duration_secs / 60;
    match tss.map(|t| t as u32) {
        Some(t) if t >= 150 => "Very hard session — prioritise recovery",
        Some(t) if t >= 100 => "Hard session — allow a day to recover",
        Some(t) if t >= 60 => "Solid effort — moderate fatigue expected",
        Some(t) if t >= 30 => "Moderate session — should feel manageable",
        Some(_) => "Light session — easy on the body",
        None if mins >= 90 => "Long session",
        None if mins >= 45 => "Moderate session",
        None => "Short session",
    }
}

pub struct HistoryPage {
    root: gtk::Box,
}

impl HistoryPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` whenever sessions may
    /// have been added or removed.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        on_toast: ToastFn,
        ftp: u32,
        weight_kg: f32,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // Import button — prominent placement at the top of the content area
        let import_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_bottom(6)
            .build();
        import_box.append(
            &gtk::Label::builder()
                .label("Import activities recorded outside the app")
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .build(),
        );
        let import_btn = gtk::Button::builder()
            .label("Import FIT File")
            .icon_name("document-open-symbolic")
            .css_classes(["pill"])
            .halign(gtk::Align::End)
            .tooltip_text("Import an activity recorded on a Garmin, Wahoo, or other device")
            .build();
        import_box.append(&import_btn);
        inner.append(&import_box);

        let list_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        inner.append(&list_box);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        // Holder so rows can call reload after delete/import without a circular Rc.
        let reload_holder: ReloadHolder = Rc::new(RefCell::new(None));

        // ── Import FIT File ───────────────────────────────────────────────────
        {
            let rh_imp = Rc::clone(&reload_holder);
            let on_imported: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(reload) = rh_imp.borrow().as_ref() {
                    reload();
                }
            });
            crate::ui::widgets::fit_import::connect_fit_import_button(
                &import_btn,
                pool.clone(),
                rt_handle.clone(),
                Rc::clone(&on_toast),
                on_imported,
            );
        }

        let reload: Rc<dyn Fn()> = {
            let list_box = list_box.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let reload_holder = Rc::clone(&reload_holder);
            let on_toast = Rc::clone(&on_toast);

            Rc::new(move || {
                // block_on is safe here: called from GLib main thread (not a tokio thread)
                let records = rt_handle
                    .block_on(db::load_session_records(&pool))
                    .unwrap_or_default();
                let icu_activities = rt_handle
                    .block_on(db::load_intervals_activities(&pool))
                    .unwrap_or_default();

                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }

                if records.is_empty() && icu_activities.is_empty() {
                    list_box.append(
                        &adw::StatusPage::builder()
                            .icon_name("document-open-recent-symbolic")
                            .title("No Sessions Yet")
                            .description(
                                "Complete a workout to see your history here, or sync \
                                 activities from Intervals.icu in Preferences.",
                            )
                            .vexpand(true)
                            .build(),
                    );
                    return;
                }

                // Build a minute-precision set of local session start times.
                // Used to suppress ICU activities that duplicate a locally recorded session.
                let local_session_minutes: std::collections::HashSet<String> = records
                    .iter()
                    .map(|r| {
                        let dt = r.session.started_at.with_timezone(&Local).naive_local();
                        format!(
                            "{}-{:02}-{:02}T{:02}:{:02}",
                            dt.year(),
                            dt.month(),
                            dt.day(),
                            dt.hour(),
                            dt.minute()
                        )
                    })
                    .collect();
                let icu_activities: Vec<_> = icu_activities
                    .into_iter()
                    .filter(|act| match act.start_datetime_local {
                        Some(dt) => !local_session_minutes.contains(&format!(
                            "{}-{:02}-{:02}T{:02}:{:02}",
                            dt.year(),
                            dt.month(),
                            dt.day(),
                            dt.hour(),
                            dt.minute()
                        )),
                        None => true,
                    })
                    .collect();

                // Merge sessions and Intervals.icu activities sorted by date descending.
                // Tuple: (date, is_icu, index) — sessions sort before icu on the same day.
                let mut indices: Vec<(NaiveDate, bool, usize)> =
                    Vec::with_capacity(records.len() + icu_activities.len());
                for (i, record) in records.iter().enumerate() {
                    let date = record.session.started_at.with_timezone(&Local).date_naive();
                    indices.push((date, false, i));
                }
                for (i, act) in icu_activities.iter().enumerate() {
                    indices.push((act.date, true, i));
                }
                indices.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

                let mut current_ym: Option<(i32, u32)> = None;
                let mut current_group: Option<adw::PreferencesGroup> = None;

                for (date, is_icu, idx) in indices.iter().copied() {
                    let ym = (date.year(), date.month());

                    if Some(ym) != current_ym {
                        if let Some(g) = current_group.take() {
                            list_box.append(&g);
                        }
                        let month_str = NaiveDate::from_ymd_opt(ym.0, ym.1, 1)
                            .map(|d| d.format("%B %Y").to_string())
                            .unwrap_or_default();
                        current_group =
                            Some(adw::PreferencesGroup::builder().title(&month_str).build());
                        current_ym = Some(ym);
                    }

                    if let Some(ref group) = current_group {
                        if is_icu {
                            group.add(&make_intervals_row(
                                &icu_activities[idx],
                                &pool,
                                &rt_handle,
                                &reload_holder,
                                &on_toast,
                                ftp,
                                weight_kg,
                            ));
                        } else {
                            group.add(&make_session_row(
                                &records[idx],
                                &pool,
                                &rt_handle,
                                &reload_holder,
                                &on_toast,
                                ftp,
                                weight_kg,
                            ));
                        }
                    }
                }

                if let Some(g) = current_group.take() {
                    list_box.append(&g);
                }
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));
        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

fn make_session_row(
    record: &SessionRecord,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    reload_holder: &ReloadHolder,
    on_toast: &ToastFn,
    ftp: u32,
    weight_kg: f32,
) -> adw::ActionRow {
    let local_dt = record.session.started_at.with_timezone(&Local);
    let title = record
        .workout_name
        .as_deref()
        .unwrap_or("Free Ride")
        .to_string();
    let dur = record.session.duration_secs() as u32;
    let activity_summary = activity_summary_text(dur, record.session.tss(ftp));
    let subtitle = format!(
        "{} — {}",
        local_dt.format("%-d %b, %H:%M"),
        activity_summary
    );

    let row = adw::ActionRow::builder()
        .title(&title)
        .subtitle(&subtitle)
        .activatable(true)
        .build();

    // Load the structured workout (if any) for interval analysis and compliance score.
    let workout_opt: Option<Workout> = record.session.workout_id.and_then(|wid| {
        rt_handle
            .block_on(db::load_workout_by_id(pool, wid))
            .ok()
            .flatten()
    });

    // ── Compliance suffix ─────────────────────────────────────────────────
    if let Some(ref wk) = workout_opt {
        if let Some(pct) = record.session.compliance_pct(&wk.segments, ftp) {
            let sep_c = gtk::Separator::builder()
                .orientation(gtk::Orientation::Vertical)
                .margin_top(12)
                .margin_bottom(12)
                .build();
            let compliance_label = gtk::Label::builder()
                .label(format!("{pct}%"))
                .css_classes(["numeric", "caption"])
                .tooltip_text("Workout compliance — % of active intervals within ±10% of target")
                .valign(gtk::Align::Center)
                .build();
            if pct >= 80 {
                compliance_label.add_css_class("success");
            } else if pct >= 60 {
                compliance_label.add_css_class("warning");
            } else {
                compliance_label.add_css_class("error");
            }
            row.add_prefix(&sep_c);
            row.add_prefix(&compliance_label);
        }
    }

    // ── Detail view on row activation ─────────────────────────────────────
    let session_detail = record.session.clone();
    let title_detail = title.clone();
    let local_dt_detail = local_dt;
    let workout_for_detail = workout_opt;
    let pool_det = pool.clone();
    let rt_det = rt_handle.clone();
    let rh_det = Rc::clone(reload_holder);
    row.connect_activated(move |row| {
        let parent = row.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        show_session_detail(
            &session_detail,
            &title_detail,
            local_dt_detail,
            ftp,
            weight_kg,
            workout_for_detail.as_ref(),
            parent.as_ref(),
            pool_det.clone(),
            rt_det.clone(),
            Rc::clone(&rh_det),
        );
    });

    let sep = || {
        gtk::Separator::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(12)
            .build()
    };

    let dur_label = gtk::Label::builder()
        .label(WorkoutEngine::format_duration(dur))
        .css_classes(["numeric", "dim-label", "caption"])
        .valign(gtk::Align::Center)
        .tooltip_text("Duration")
        .build();
    row.add_suffix(&sep());
    row.add_suffix(&dur_label);

    let power_str = match record.session.normalised_power() {
        Some(np) => format!("{} W NP", np as u32),
        None => match record.session.average_power() {
            Some(avg) => format!("{} W avg", avg as u32),
            None => "—".to_string(),
        },
    };
    let power_label = gtk::Label::builder()
        .label(&power_str)
        .css_classes(["numeric", "dim-label", "caption"])
        .valign(gtk::Align::Center)
        .tooltip_text("Normalised power (or average if NP unavailable)")
        .build();
    row.add_suffix(&sep());
    row.add_suffix(&power_label);

    let kj_label = gtk::Label::builder()
        .label(format!("{:.0} kJ", record.session.kilojoules()))
        .css_classes(["numeric", "dim-label", "caption"])
        .valign(gtk::Align::Center)
        .tooltip_text("Total energy output")
        .build();
    row.add_suffix(&sep());
    row.add_suffix(&kj_label);

    // ── RPE indicator / Rate button ───────────────────────────────────────────
    row.add_suffix(&sep());
    if let Some(rpe) = record.session.rpe {
        let rpe_label = match rpe {
            1 => "Very Easy",
            2 => "Easy",
            3 => "Moderate",
            4 => "Hard",
            5 => "Very Hard",
            _ => "Maximum Effort",
        };
        if let Some(texture) = crate::ui::resources::rpe_texture(rpe) {
            let image = gtk::Image::builder()
                .paintable(&texture)
                .pixel_size(28)
                .valign(gtk::Align::Center)
                .tooltip_text(format!("RPE {rpe}/6 — {rpe_label}"))
                .build();
            row.add_suffix(&image);
        } else {
            let rpe_chip = gtk::Label::builder()
                .label(format!("RPE {rpe}"))
                .css_classes(["caption", "dim-label", "numeric"])
                .valign(gtk::Align::Center)
                .tooltip_text(format!("RPE {rpe}/6 — {rpe_label}"))
                .build();
            row.add_suffix(&rpe_chip);
        }
    } else {
        let session_id_rate = record.session.id;
        let pool_rate = pool.clone();
        let rt_rate = rt_handle.clone();
        let rh_rate = Rc::clone(reload_holder);
        let rate_btn = gtk::Button::builder()
            .icon_name("starred-symbolic")
            .tooltip_text("Rate effort — add self-evaluation for this session")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        rate_btn.connect_clicked(move |btn| {
            let pool = pool_rate.clone();
            let rt = rt_rate.clone();
            let rh = Rc::clone(&rh_rate);
            crate::ui::widgets::rpe_dialog::show(btn, move |rpe| {
                let pool = pool.clone();
                let rh = Rc::clone(&rh);
                crate::ui::spawn_to_main(
                    &rt,
                    async move { db::save_session_rpe(&pool, session_id_rate, rpe).await },
                    move |res| {
                        if let Err(e) = res {
                            tracing::error!("save_session_rpe failed: {e}");
                        }
                        if let Some(reload) = rh.borrow().as_ref() {
                            reload();
                        }
                    },
                );
            });
        });
        row.add_suffix(&rate_btn);
    }

    // ── Export FIT ────────────────────────────────────────────────────────────
    let session_export = record.session.clone();
    let on_toast_export = Rc::clone(on_toast);
    let export_btn = gtk::Button::builder()
        .icon_name("document-send-symbolic")
        .tooltip_text("Export session as FIT file")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();
    export_btn.connect_clicked(move |_| {
        match crate::data::fit::export_to_xdg_path(&session_export) {
            Ok(path) => {
                tracing::info!("Exported FIT: {}", path.display());
                let file_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let folder_uri = path
                    .parent()
                    .map(|p| format!("file://{}", p.display()))
                    .unwrap_or_default();

                let toast = adw::Toast::builder()
                    .title(format!("Exported: {file_name}"))
                    .button_label("Open folder")
                    .timeout(6)
                    .build();
                toast.connect_button_clicked(move |_| {
                    let file = gio::File::for_parse_name(&folder_uri);
                    gtk::FileLauncher::new(Some(&file)).launch(
                        None::<&gtk::Window>,
                        None::<&gio::Cancellable>,
                        |_| {},
                    );
                });
                on_toast_export(toast);
            }
            Err(e) => {
                tracing::error!("FIT export failed: {e}");
                on_toast_export(
                    adw::Toast::builder()
                        .title(format!("Export failed: {e}"))
                        .timeout(8)
                        .build(),
                );
            }
        }
    });
    row.add_suffix(&sep());
    row.add_suffix(&export_btn);

    // ── Delete session ────────────────────────────────────────────────────────
    let session_id = record.session.id;
    let pool_del = pool.clone();
    let rt_del = rt_handle.clone();
    let rh_del = Rc::clone(reload_holder);

    let delete_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete this session")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();

    delete_btn.connect_clicked(move |btn| {
        let pool = pool_del.clone();
        let rt = rt_del.clone();
        let rh = Rc::clone(&rh_del);
        crate::ui::widgets::dialog::confirm_destructive(
            btn,
            "Delete Session?",
            "This session and all its data will be permanently deleted.",
            "_Delete",
            move || {
                let pool = pool.clone();
                let rh = Rc::clone(&rh);
                crate::ui::spawn_to_main(
                    &rt,
                    async move { db::delete_session(&pool, session_id).await },
                    move |res| {
                        if let Err(e) = res {
                            tracing::error!("delete_session failed: {e}");
                        }
                        if let Some(reload) = rh.borrow().as_ref() {
                            reload();
                        }
                    },
                );
            },
        );
    });

    row.add_suffix(&sep());
    row.add_suffix(&delete_btn);

    row
}

fn make_intervals_row(
    act: &db::IntervalsActivity,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    reload_holder: &ReloadHolder,
    on_toast: &ToastFn,
    ftp: u32,
    weight_kg: f32,
) -> adw::ActionRow {
    let fallback_name = if is_run(&act.sport_type) {
        "Run"
    } else {
        "Ride"
    };
    let name = if act.name.is_empty() {
        fallback_name
    } else {
        &act.name
    };

    let subtitle = if let Some(dt) = act.start_datetime_local {
        dt.format("%-d %b, %H:%M").to_string()
    } else {
        act.date.format("%-d %b").to_string()
    };

    let row = adw::ActionRow::builder()
        .title(name)
        .subtitle(&subtitle)
        .activatable(true)
        .build();

    row.add_prefix(&sport_badge(&act.sport_type));

    let act_clone = act.clone();
    let title_clone = name.to_string();
    let pool_detail = pool.clone();
    let rt_detail = rt_handle.clone();
    row.connect_activated(move |row| {
        let parent = row.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        show_intervals_detail(
            &act_clone,
            &title_clone,
            ftp,
            weight_kg,
            &pool_detail,
            &rt_detail,
            parent.as_ref(),
        );
    });

    let sep = || {
        gtk::Separator::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(12)
            .build()
    };

    if let Some(dur) = act.duration_secs {
        let dur_label = gtk::Label::builder()
            .label(WorkoutEngine::format_duration(dur))
            .css_classes(["numeric", "dim-label", "caption"])
            .valign(gtk::Align::Center)
            .tooltip_text("Duration")
            .build();
        row.add_suffix(&sep());
        row.add_suffix(&dur_label);
    }

    if is_run(&act.sport_type) {
        // Running: show distance and pace instead of power
        if let Some(dist) = act.distance_m {
            let dist_label = gtk::Label::builder()
                .label(format_distance(dist))
                .css_classes(["numeric", "dim-label", "caption"])
                .valign(gtk::Align::Center)
                .tooltip_text("Distance")
                .build();
            row.add_suffix(&sep());
            row.add_suffix(&dist_label);
        }
        if let (Some(dist), Some(dur)) = (act.distance_m, act.duration_secs) {
            let pace_label = gtk::Label::builder()
                .label(format_pace(dist, dur))
                .css_classes(["numeric", "dim-label", "caption"])
                .valign(gtk::Align::Center)
                .tooltip_text("Average pace")
                .build();
            row.add_suffix(&sep());
            row.add_suffix(&pace_label);
        }
    } else {
        // Cycling / other: show power
        if let Some(w) = act.average_watts {
            let power_label = gtk::Label::builder()
                .label(format!("{w} W avg"))
                .css_classes(["numeric", "dim-label", "caption"])
                .valign(gtk::Align::Center)
                .tooltip_text("Average power")
                .build();
            row.add_suffix(&sep());
            row.add_suffix(&power_label);
        }
    }

    if let Some(hr) = act.average_hr {
        let hr_label = gtk::Label::builder()
            .label(format!("{hr} bpm"))
            .css_classes(["numeric", "dim-label", "caption"])
            .valign(gtk::Align::Center)
            .tooltip_text("Average heart rate")
            .build();
        row.add_suffix(&sep());
        row.add_suffix(&hr_label);
    }

    if let Some(elev) = act.elevation_gain_m.filter(|&e| e >= 5.0) {
        let elev_label = gtk::Label::builder()
            .label(format!("↑{:.0}m", elev))
            .css_classes(["numeric", "dim-label", "caption"])
            .valign(gtk::Align::Center)
            .tooltip_text("Elevation gain")
            .build();
        row.add_suffix(&sep());
        row.add_suffix(&elev_label);
    }

    if let Some(tss) = act.tss {
        let tss_label = gtk::Label::builder()
            .label(format!("TSS {:.0}", tss))
            .css_classes(["numeric", "dim-label", "caption"])
            .valign(gtk::Align::Center)
            .tooltip_text("Training Stress Score")
            .build();
        row.add_suffix(&sep());
        row.add_suffix(&tss_label);
    }

    // Cloud icon signals "synced from external source" without cluttering the subtitle
    row.add_suffix(&sep());
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("network-wireless-symbolic")
            .tooltip_text("Synced from Intervals.icu — read only")
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );

    let icu_id = act.icu_id.clone();
    let pool_del = pool.clone();
    let rt_del = rt_handle.clone();
    let rh_del = Rc::clone(reload_holder);
    let on_toast_del = Rc::clone(on_toast);

    let delete_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove this activity from local history")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();

    delete_btn.connect_clicked(move |btn| {
        let pool = pool_del.clone();
        let rt = rt_del.clone();
        let rh = Rc::clone(&rh_del);
        let on_toast = Rc::clone(&on_toast_del);
        let icu_id = icu_id.clone();
        crate::ui::widgets::dialog::confirm_destructive(
            btn,
            "Remove Activity?",
            "This will remove the activity from Cycle's local history. \
             It will not be deleted from Intervals.icu.",
            "_Remove",
            move || {
                let pool = pool.clone();
                let rh = Rc::clone(&rh);
                let on_toast = Rc::clone(&on_toast);
                let icu_id = icu_id.clone();
                crate::ui::spawn_to_main(
                    &rt,
                    async move { db::delete_intervals_activity(&pool, &icu_id).await },
                    move |res| {
                        if let Err(e) = res {
                            tracing::error!("delete_intervals_activity failed: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title("Failed to remove activity")
                                    .timeout(4)
                                    .build(),
                            );
                        } else {
                            on_toast(
                                adw::Toast::builder()
                                    .title("Activity removed from local history")
                                    .timeout(3)
                                    .build(),
                            );
                            if let Some(reload) = rh.borrow().as_ref() {
                                reload();
                            }
                        }
                    },
                );
            },
        );
    });

    row.add_suffix(&sep());
    row.add_suffix(&delete_btn);

    row
}

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
        .icon_name("x-office-calendar-symbolic")
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
            stats_group.add(&make_row("Avg Pace", format_pace(dist, dur)));
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
            let athlete_id = match rt
                .block_on(db::get_setting(&pool, "intervals.athlete_id"))
                .unwrap_or(None)
            {
                Some(s) if !s.is_empty() => s,
                _ => {
                    status_label
                        .set_label("Intervals.icu credentials not set — configure in Preferences");
                    status_label.set_visible(true);
                    return;
                }
            };
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
                let athlete_id = athlete_id.clone();
                let api_key = api_key.clone();
                let icu_id = icu_id.clone();
                async move {
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
                    rt.block_on(db::save_activity_streams(&pool, &icu_id, &json))
                        .ok();
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

    // Initial DB cache check (fast synchronous read — safe on GLib main thread).
    // If cached data exists, display it immediately; then re-fetch in the background if GPS is
    // absent (stale cache from before GPS support was added, or an activity whose streams
    // endpoint returned 404 and was never retried with the map endpoint).
    match rt_handle
        .block_on(db::get_activity_streams(pool, &icu_id))
        .unwrap_or(None)
    {
        Some(json) => match ActivityStreams::from_json(&json) {
            Some(s) => {
                let needs_gps_refresh = !s.has_gps();
                populate_streams(s);
                if needs_gps_refresh {
                    // Cache has no GPS — re-fetch to pick up the map endpoint data.
                    do_fetch();
                }
            }
            None => {
                // Corrupt cache — re-fetch.
                do_fetch();
            }
        },
        None => {
            // Nothing cached yet — fetch on first open.
            do_fetch();
        }
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
        .icon_name("x-office-calendar-symbolic")
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
            zone_secs[zone_index(watts, ftp)] += 1;
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

        let zone_bar = gtk::DrawingArea::builder()
            .content_height(20)
            .hexpand(true)
            .build();
        let zs = zone_secs;
        zone_bar.set_draw_func(move |_widget, cr, width, height| {
            let total: u32 = zs.iter().sum();
            if total == 0 {
                return;
            }
            let w = width as f64;
            let h = height as f64;
            let mut x = 0.0f64;
            for (i, &secs) in zs.iter().enumerate() {
                if secs == 0 {
                    continue;
                }
                let seg_w = (secs as f64 / total as f64) * w;
                let (r, g, b) = zone_color(i);
                cr.set_source_rgba(r, g, b, 0.85);
                cr.rectangle(x, 0.0, seg_w, h);
                cr.fill().ok();
                x += seg_w;
            }
        });
        zone_section.append(&zone_bar);

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
        let win_eval = win.clone();

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

        rate_row.connect_activated(move |row| {
            let pool = pool_eval.clone();
            let rt = rt_eval.clone();
            let rh = Rc::clone(&rh_eval);
            let win = win_eval.clone();
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
        });

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
