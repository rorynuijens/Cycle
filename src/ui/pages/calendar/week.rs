//! The week view: one row per day, with the week's load totals in the gutter.

use adw::prelude::*;
use chrono::{Duration, Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::calendar::{group_by_date, CalendarEvent};
use crate::data::db;
use crate::data::workout::Workout;
use crate::ui::widgets::zone_color::{category_zone_rgb, color_stripe};

use super::ReloadFn;

#[allow(clippy::too_many_arguments)]
pub fn build_week_view(
    week_start: NaiveDate,
    events: &[CalendarEvent],
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    reload_holder: Rc<RefCell<Option<ReloadFn>>>,
    workouts: Rc<Vec<Workout>>,
    on_start_workout: Rc<dyn Fn(Workout)>,
    on_start_route: crate::ui::StartRouteHolder,
    ftp: u32,
    weight_kg: f32,
    on_toast: Rc<dyn Fn(adw::Toast)>,
) -> gtk::Box {
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    let by_day = group_by_date(events);
    let today = Local::now().date_naive();

    for i in 0..7i64 {
        let day = week_start + Duration::days(i);
        let day_events = by_day.get(&day).map(Vec::as_slice).unwrap_or(&[]);
        let is_today = day == today;
        let is_past = day < today;

        let day_frame = gtk::Frame::new(None);
        if is_today {
            day_frame.add_css_class("card");
        }

        let hbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let date_label = gtk::Label::builder()
            .label(day.format("%A\n%-d %b").to_string())
            .css_classes(if is_today {
                vec!["caption-heading", "accent"]
            } else if is_past {
                vec!["caption", "dim-label"]
            } else {
                vec!["caption"]
            })
            .valign(gtk::Align::Center)
            .width_chars(10)
            .xalign(0.0)
            .build();
        hbox.append(&date_label);

        let sep = gtk::Separator::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        hbox.append(&sep);

        // Check for time off — show it as a small indicator but don't skip other events.
        // A day can be marked time off AND still have recorded activities (e.g. a run).
        let time_off_entry = day_events.iter().find_map(|e| {
            if let CalendarEvent::TimeOff(t) = e {
                Some(t)
            } else {
                None
            }
        });

        let non_time_off_events: Vec<&CalendarEvent> = day_events
            .iter()
            .copied()
            .filter(|e| !matches!(e, CalendarEvent::TimeOff(_)))
            .collect();

        // Build the content column (time-off badge + any real events)
        let chip_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .build();

        if let Some(time_off) = time_off_entry {
            let time_off_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            let time_off_label = gtk::Label::builder()
                .label(if time_off.notes.is_empty() {
                    "Time off".to_string()
                } else {
                    format!("Time off — {}", time_off.notes)
                })
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .build();
            time_off_row.append(&time_off_label);

            let date_for_remove = day;
            let pool_to = pool.clone();
            let rt_to = rt_handle.clone();
            let rh_to = Rc::clone(&reload_holder);
            let remove_to_btn = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .css_classes(["flat", "circular"])
                .tooltip_text("Remove time off")
                .valign(gtk::Align::Center)
                .build();
            remove_to_btn.connect_clicked(move |_| {
                let pool_to = pool_to.clone();
                let rh_to = Rc::clone(&rh_to);
                crate::ui::spawn_to_main(
                    &rt_to,
                    async move { db::delete_time_off(&pool_to, date_for_remove).await },
                    move |res| {
                        if let Err(e) = res {
                            tracing::error!("delete_time_off: {e}");
                        }
                        if let Some(reload) = rh_to.borrow().as_ref() {
                            reload();
                        }
                    },
                );
            });
            time_off_row.append(&remove_to_btn);
            chip_box.append(&time_off_row);
        }

        if non_time_off_events.is_empty() && time_off_entry.is_none() {
            if is_past {
                chip_box.append(
                    &gtk::Label::builder()
                        .label("No activity")
                        .css_classes(["caption", "dim-label"])
                        .halign(gtk::Align::Start)
                        .hexpand(true)
                        .build(),
                );
            } else {
                // An empty future day is an invitation to schedule.
                let rest_btn = gtk::Button::builder()
                    .css_classes(["flat"])
                    .halign(gtk::Align::Start)
                    .hexpand(true)
                    .tooltip_text("Schedule a workout for this day")
                    .build();
                let rest_content = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(6)
                    .build();
                rest_content.append(
                    &gtk::Image::builder()
                        .icon_name("list-add-symbolic")
                        .css_classes(["dim-label"])
                        .build(),
                );
                rest_content.append(
                    &gtk::Label::builder()
                        .label("Rest — tap to schedule")
                        .css_classes(["caption", "dim-label"])
                        .build(),
                );
                rest_btn.set_child(Some(&rest_content));

                let day_sched = day;
                let pool_rs = pool.clone();
                let rt_rs = rt_handle.clone();
                let rh_rs = Rc::clone(&reload_holder);
                let workouts_rs = Rc::clone(&workouts);
                rest_btn.connect_clicked(move |btn| {
                    let rh_c = Rc::clone(&rh_rs);
                    let reload_fn: Rc<dyn Fn()> = Rc::new(move || {
                        if let Some(f) = rh_c.borrow().as_ref() {
                            f();
                        }
                    });
                    super::dialogs::show_schedule_dialog(
                        btn,
                        &workouts_rs,
                        day_sched,
                        pool_rs.clone(),
                        rt_rs.clone(),
                        ftp,
                        weight_kg,
                        reload_fn,
                    );
                });
                chip_box.append(&rest_btn);
            }
        } else {
            for event in &non_time_off_events {
                match event {
                    CalendarEvent::TimeOff(_) => unreachable!(),

                    CalendarEvent::Scheduled(entry) => {
                        let entry_row = gtk::Box::builder()
                            .orientation(gtk::Orientation::Horizontal)
                            .spacing(6)
                            .build();

                        entry_row.append(&color_stripe(category_zone_rgb(&entry.category)));

                        // Drag to another day. Only open plans move: a ridden
                        // session belongs to the day it was ridden on. Same shape
                        // as the segment reorder in library/editor.rs, with the
                        // entry id as the payload instead of a row index.
                        if !entry.completed {
                            let drag = gtk::DragSource::new();
                            drag.set_actions(gtk::gdk::DragAction::MOVE);
                            let dragged_id = entry.id;
                            drag.connect_prepare(move |_, _, _| {
                                Some(gtk::gdk::ContentProvider::for_value(&dragged_id.to_value()))
                            });
                            entry_row.add_controller(drag);
                            entry_row.set_tooltip_text(Some("Drag to move to another day"));
                        }

                        let display_name = if entry.completed {
                            format!("✓  {}", entry.item.name())
                        } else {
                            entry.item.name().to_string()
                        };
                        let dur_mins = entry.duration_secs / 60;
                        let tooltip = format!(
                            "{} min · TSS {:.0} · {}",
                            dur_mins,
                            entry.tss,
                            entry.category.label()
                        );

                        let btn = gtk::Button::builder()
                            .css_classes(["flat"])
                            .halign(gtk::Align::Start)
                            .hexpand(true)
                            .tooltip_text(&tooltip)
                            .build();
                        let lbl = gtk::Label::builder()
                            .label(&display_name)
                            .halign(gtk::Align::Start)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(if entry.completed {
                                vec!["caption", "dim-label"]
                            } else {
                                vec!["caption"]
                            })
                            .build();
                        btn.set_child(Some(&lbl));

                        let entry_clone = (*entry).clone();
                        let pool_d = pool.clone();
                        let rt_d = rt_handle.clone();
                        let workouts_d = Rc::clone(&workouts);
                        let on_start_d = Rc::clone(&on_start_workout);
                        let on_start_route_d = Rc::clone(&on_start_route);
                        let rh_d = Rc::clone(&reload_holder);
                        btn.connect_clicked(move |b| {
                            let rh_c = Rc::clone(&rh_d);
                            let reload_fn: Rc<dyn Fn()> = Rc::new(move || {
                                if let Some(f) = rh_c.borrow().as_ref() {
                                    f();
                                }
                            });
                            super::dialogs::show_workout_detail_dialog(
                                b,
                                &entry_clone,
                                pool_d.clone(),
                                rt_d.clone(),
                                Rc::clone(&workouts_d),
                                Rc::clone(&on_start_d),
                                Rc::clone(&on_start_route_d),
                                reload_fn,
                            );
                        });

                        entry_row.append(&btn);

                        // Load out of the tooltip and into view
                        entry_row.append(
                            &gtk::Label::builder()
                                .label(format!("{} min · TSS {:.0}", dur_mins, entry.tss))
                                .css_classes(["caption", "dim-label", "numeric"])
                                .halign(gtk::Align::End)
                                .build(),
                        );

                        chip_box.append(&entry_row);
                    }

                    CalendarEvent::Session(session, workout_name) => {
                        let entry_row = gtk::Box::builder()
                            .orientation(gtk::Orientation::Horizontal)
                            .spacing(6)
                            .build();

                        entry_row.append(&color_stripe(None));

                        let display_name = workout_name
                            .as_deref()
                            .filter(|n| !n.is_empty())
                            .unwrap_or("Unstructured Ride");
                        let dur_mins = session.session.duration_secs() / 60;
                        let session_tss = session.session.tss(ftp);

                        let btn = gtk::Button::builder()
                            .css_classes(["flat"])
                            .halign(gtk::Align::Start)
                            .hexpand(true)
                            .tooltip_text(format!("{} min · In-app session", dur_mins))
                            .build();
                        btn.set_child(Some(
                            &gtk::Label::builder()
                                .label(display_name)
                                .halign(gtk::Align::Start)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .css_classes(["caption", "dim-label"])
                                .build(),
                        ));

                        let session_det = (*session).clone();
                        let title_det = workout_name
                            .as_deref()
                            .filter(|n| !n.is_empty())
                            .unwrap_or("Unstructured Ride")
                            .to_string();
                        let pool_det = pool.clone();
                        let rt_det = rt_handle.clone();
                        let rh_det = Rc::clone(&reload_holder);
                        btn.connect_clicked(move |b| {
                            let local_dt =
                                session_det.session.started_at.with_timezone(&chrono::Local);
                            let parent = b.root().and_downcast::<gtk::Window>();

                            // The ride's workout plan is fetched off the main
                            // thread; a block_on here stalled the GLib loop
                            // between the click and the dialog (CLAUDE.md §2.3).
                            let pool_w = pool_det.clone();
                            let workout_id = session_det.session.workout_id;
                            let session_w = session_det.clone();
                            let title_w = title_det.clone();
                            let pool_show = pool_det.clone();
                            let rt_show = rt_det.clone();
                            let rh_show = Rc::clone(&rh_det);
                            crate::ui::spawn_to_main(
                                &rt_det,
                                async move {
                                    match workout_id {
                                        Some(wid) => db::load_workout_by_id(&pool_w, wid)
                                            .await
                                            .ok()
                                            .flatten(),
                                        None => None,
                                    }
                                },
                                move |workout| {
                                    // A missing plan is normal (route rides
                                    // have none) — the dialog opens either way.
                                    super::detail::show_session_detail(
                                        &session_w.session,
                                        &title_w,
                                        local_dt,
                                        ftp,
                                        weight_kg,
                                        workout.as_ref(),
                                        parent.as_ref(),
                                        pool_show,
                                        rt_show,
                                        rh_show,
                                    );
                                },
                            );
                        });

                        entry_row.append(&btn);

                        entry_row.append(
                            &gtk::Label::builder()
                                .label(match session_tss {
                                    Some(t) => format!("{} min · TSS {:.0}", dur_mins, t),
                                    None => format!("{} min", dur_mins),
                                })
                                .css_classes(["caption", "dim-label", "numeric"])
                                .halign(gtk::Align::End)
                                .build(),
                        );

                        // Delete button for local sessions
                        let session_id_del = session.session.id;
                        let pool_del = pool.clone();
                        let rt_del = rt_handle.clone();
                        let rh_del = Rc::clone(&reload_holder);
                        let del_btn = gtk::Button::builder()
                            .icon_name("user-trash-symbolic")
                            .tooltip_text("Delete this session")
                            .css_classes(["flat", "circular", "destructive-action"])
                            .valign(gtk::Align::Center)
                            .build();
                        del_btn.connect_clicked(move |btn| {
                            let pool_c = pool_del.clone();
                            let rt_c = rt_del.clone();
                            let rh_c = Rc::clone(&rh_del);
                            crate::ui::widgets::dialog::confirm_destructive(
                                btn,
                                "Delete Session?",
                                "This session and all its data will be permanently deleted.",
                                "_Delete",
                                move || {
                                    let pool_c = pool_c.clone();
                                    let rh_c = Rc::clone(&rh_c);
                                    crate::ui::spawn_to_main(
                                        &rt_c,
                                        async move {
                                            db::delete_session(&pool_c, session_id_del).await
                                        },
                                        move |res| {
                                            if let Err(e) = res {
                                                tracing::error!("delete_session: {e}");
                                            }
                                            if let Some(f) = rh_c.borrow().as_ref() {
                                                f();
                                            }
                                        },
                                    );
                                },
                            );
                        });
                        entry_row.append(&del_btn);

                        chip_box.append(&entry_row);
                    }

                    CalendarEvent::IcuActivity(activity) => {
                        let entry_row = gtk::Box::builder()
                            .orientation(gtk::Orientation::Horizontal)
                            .spacing(6)
                            .build();

                        entry_row.append(&color_stripe(None));
                        // Sport icon kept — it distinguishes rides from runs etc.
                        let sport_icon =
                            crate::ui::resources::sport_icon(&activity.sport_type, false);
                        sport_icon.add_css_class("dim-label");
                        entry_row.append(&sport_icon);

                        let display_name = if activity.name.trim().is_empty() {
                            activity.sport_type.clone()
                        } else {
                            activity.name.clone()
                        };
                        let dur_str = activity
                            .duration_secs
                            .map(|s| format!("{} min", s / 60))
                            .unwrap_or_else(|| "—".to_string());

                        let btn = gtk::Button::builder()
                            .css_classes(["flat"])
                            .halign(gtk::Align::Start)
                            .hexpand(true)
                            .tooltip_text(format!("{} · Intervals.icu", dur_str))
                            .build();
                        btn.set_child(Some(
                            &gtk::Label::builder()
                                .label(&display_name)
                                .halign(gtk::Align::Start)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .css_classes(["caption", "dim-label"])
                                .build(),
                        ));

                        let activity_det = (*activity).clone();
                        let display_det = display_name.clone();
                        let pool_det = pool.clone();
                        let rt_det = rt_handle.clone();
                        btn.connect_clicked(move |b| {
                            let parent = b.root().and_downcast::<gtk::Window>();
                            super::detail::show_intervals_detail(
                                &activity_det,
                                &display_det,
                                ftp,
                                weight_kg,
                                &pool_det,
                                &rt_det,
                                parent.as_ref(),
                            );
                        });

                        entry_row.append(&btn);

                        entry_row.append(
                            &gtk::Label::builder()
                                .label(match activity.tss {
                                    Some(t) => format!("{} · TSS {:.0}", dur_str, t),
                                    None => dur_str.clone(),
                                })
                                .css_classes(["caption", "dim-label", "numeric"])
                                .halign(gtk::Align::End)
                                .build(),
                        );

                        // Delete button for ICU activities
                        let icu_id_del = activity.icu_id.clone();
                        let pool_del = pool.clone();
                        let rt_del = rt_handle.clone();
                        let rh_del = Rc::clone(&reload_holder);
                        let on_toast_del = Rc::clone(&on_toast);
                        let del_btn = gtk::Button::builder()
                            .icon_name("user-trash-symbolic")
                            .tooltip_text("Remove this activity from local history")
                            .css_classes(["flat", "circular", "destructive-action"])
                            .valign(gtk::Align::Center)
                            .build();
                        del_btn.connect_clicked(move |btn| {
                            let pool_c = pool_del.clone();
                            let rt_c = rt_del.clone();
                            let rh_c = Rc::clone(&rh_del);
                            let toast_c = Rc::clone(&on_toast_del);
                            let icu_id_c = icu_id_del.clone();
                            crate::ui::widgets::dialog::confirm_destructive(
                                btn,
                                "Remove Activity?",
                                "This will remove the activity from Cycle's local history. \
                                 It will not be deleted from Intervals.icu.",
                                "_Remove",
                                move || {
                                    let pool_c = pool_c.clone();
                                    let rh_c = Rc::clone(&rh_c);
                                    let toast_c = Rc::clone(&toast_c);
                                    let icu_id_c = icu_id_c.clone();
                                    crate::ui::spawn_to_main(
                                        &rt_c,
                                        async move {
                                            db::delete_intervals_activity(&pool_c, &icu_id_c).await
                                        },
                                        move |res| {
                                            if let Err(e) = res {
                                                tracing::error!("delete_intervals_activity: {e}");
                                                toast_c(
                                                    adw::Toast::builder()
                                                        .title("Failed to remove activity")
                                                        .timeout(4)
                                                        .build(),
                                                );
                                            } else {
                                                toast_c(
                                                    adw::Toast::builder()
                                                        .title(
                                                            "Activity removed from local history",
                                                        )
                                                        .timeout(3)
                                                        .build(),
                                                );
                                                if let Some(f) = rh_c.borrow().as_ref() {
                                                    f();
                                                }
                                            }
                                        },
                                    );
                                },
                            );
                        });
                        entry_row.append(&del_btn);

                        chip_box.append(&entry_row);
                    }
                }
            }
        }

        hbox.append(&chip_box);
        // Drop a dragged plan onto this day to move it here. The target is the
        // whole day frame rather than the entry column, so an empty day is just
        // as easy to hit as a full one.
        {
            let drop = gtk::DropTarget::new(i64::static_type(), gtk::gdk::DragAction::MOVE);
            let pool_dt = pool.clone();
            let rt_dt = rt_handle.clone();
            let rh_dt = Rc::clone(&reload_holder);
            let target_date = day.format("%Y-%m-%d").to_string();
            drop.connect_drop(move |_, value, _, _| {
                let Ok(entry_id) = value.get::<i64>() else {
                    return false;
                };
                let pool_c = pool_dt.clone();
                let date = target_date.clone();
                let rh_c = Rc::clone(&rh_dt);
                crate::ui::spawn_to_main(
                    &rt_dt,
                    async move { db::reschedule_entry(&pool_c, entry_id, &date).await },
                    move |res| {
                        match res {
                            Ok(true) => {}
                            Ok(false) => tracing::warn!("entry {entry_id} was not moved"),
                            Err(e) => tracing::error!("reschedule_entry: {e}"),
                        }
                        // Clone the callback out before running it: it rebuilds
                        // the widget this handler is attached to (CLAUDE.md §2.4).
                        let reload = rh_c.borrow().clone();
                        if let Some(reload) = reload {
                            reload();
                        }
                    },
                );
                true
            });
            day_frame.add_controller(drop);
        }

        day_frame.set_child(Some(&hbox));
        vbox.append(&day_frame);
    }

    vbox
}

// ── Month grid ────────────────────────────────────────────────────────────

/// The weekly totals gutter: planned + completed TSS and how much of the
/// plan got done — the summary cards, condensed to where they're relevant.
pub fn week_gutter(week: &crate::data::calendar::LoadTotals) -> gtk::Box {
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    let total = week.total_tss();
    if total > 0.0 {
        vbox.append(
            &gtk::Label::builder()
                .label(format!("{:.0} TSS", total))
                .css_classes(["caption-heading", "numeric"])
                .halign(gtk::Align::Start)
                .build(),
        );
    }
    if week.scheduled > 0 {
        vbox.append(
            &gtk::Label::builder()
                .label(format!("{}/{} done", week.scheduled_done, week.scheduled))
                .css_classes(["caption", "dim-label", "numeric"])
                .halign(gtk::Align::Start)
                .build(),
        );
    }
    if week.is_empty() {
        vbox.append(
            &gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .build(),
        );
    }
    vbox
}
