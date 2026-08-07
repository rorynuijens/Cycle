//! The workout editor: building a custom workout out of segments.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::db;
use crate::data::workout::{Workout, WorkoutCategory};
use crate::ui::widgets::workout_graph::WorkoutGraph;

use super::{RebuildHolder, SegmentDraft};

/// Open the workout editor dialog. Pass `Some(workout)` to edit an existing workout,
/// or `None` to create a new one.
#[allow(clippy::too_many_arguments)] // editor dialog wiring; grouping deferred
pub fn show_workout_editor(
    parent: Option<&gtk::Window>,
    pool: sqlx::SqlitePool,
    rt_handle: tokio::runtime::Handle,
    ftp: u32,
    workouts_list: Rc<RefCell<Vec<Workout>>>,
    rebuild: Rc<dyn Fn()>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    existing: Option<Workout>,
) {
    use crate::ui::widgets::workout_graph::WorkoutGraph;

    // Extract what we need up-front so `existing` doesn't need to reach into closures.
    let existing_id: Option<i64> = existing.as_ref().map(|w| w.id);
    let initial_name = existing
        .as_ref()
        .map(|w| w.name.as_str())
        .unwrap_or("Custom Workout")
        .to_string();
    let initial_desc = existing
        .as_ref()
        .map(|w| w.description.as_str())
        .unwrap_or("")
        .to_string();
    let initial_segments: Vec<(u32, f32, f32, String)> = existing
        .as_ref()
        .map(|w| {
            w.segments
                .iter()
                .map(|s| {
                    (
                        s.duration_secs,
                        s.power_low_pct,
                        s.power_high_pct,
                        s.label.clone().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                (600, 40.0, 60.0, "Warm-up".to_string()),
                (1800, 75.0, 75.0, "Main Set".to_string()),
                (300, 40.0, 40.0, "Cool-down".to_string()),
            ]
        });

    let win = adw::Window::builder()
        .modal(true)
        .title(if existing_id.is_some() {
            "Edit Workout"
        } else {
            "New Workout"
        })
        .default_width(520)
        .default_height(680)
        .build();
    win.set_transient_for(parent);

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();

    let cancel_btn = gtk::Button::builder()
        .label("Cancel")
        .css_classes(["flat"])
        .tooltip_text("Discard and close")
        .build();
    let save_btn = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .tooltip_text("Save this workout to the library")
        .build();
    header.pack_start(&cancel_btn);
    header.pack_end(&save_btn);
    toolbar_view.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(480)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // ── Name / description ────────────────────────────────────────────────
    let meta_group = adw::PreferencesGroup::builder().title("Workout").build();

    let name_row = adw::EntryRow::builder().title("Name").build();
    name_row.set_text(&initial_name);
    meta_group.add(&name_row);

    let desc_row = adw::EntryRow::builder().title("Description").build();
    desc_row.set_text(&initial_desc);
    meta_group.add(&desc_row);

    inner.append(&meta_group);

    // ── Segment list ──────────────────────────────────────────────────────
    let segments_group = adw::PreferencesGroup::builder().title("Segments").build();
    inner.append(&segments_group);

    // Draft segment state — list of (duration_secs, power_low_pct, power_high_pct, label)
    let draft: SegmentDraft = Rc::new(RefCell::new(initial_segments));

    let preview_workout = build_draft_workout(&draft.borrow(), "Custom Workout");

    // Live preview graph — defined early so rebuild_segments can update it.
    let graph_for_update: Rc<WorkoutGraph> = Rc::new(WorkoutGraph::new(&preview_workout, ftp));

    // Track added expander rows so we can remove only our rows from PreferencesGroup.
    let added_rows: Rc<RefCell<Vec<adw::ExpanderRow>>> = Rc::new(RefCell::new(Vec::new()));

    // Self-referential holder so delete and DnD drop callbacks can call rebuild_segments.
    let rebuild_segs_holder: RebuildHolder = Rc::new(RefCell::new(None));

    let rebuild_segments = segments_rebuild_closure(
        segments_group.clone(),
        Rc::clone(&draft),
        Rc::clone(&added_rows),
        Rc::clone(&graph_for_update),
        name_row.clone(),
        Rc::clone(&rebuild_segs_holder),
    );

    // Seal the holder so delete/DnD callbacks can resolve it.
    *rebuild_segs_holder.borrow_mut() = Some(Rc::clone(&rebuild_segments));

    rebuild_segments();

    // ── Add Segment button ────────────────────────────────────────────────
    let add_seg_btn = gtk::Button::builder()
        .label("Add Segment")
        .icon_name("list-add-symbolic")
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .tooltip_text("Add a new segment to the workout")
        .build();

    {
        let draft_add = Rc::clone(&draft);
        let rebuild_add = Rc::clone(&rebuild_segments);
        add_seg_btn.connect_clicked(move |_| {
            draft_add
                .borrow_mut()
                .push((300, 75.0, 75.0, String::new()));
            rebuild_add();
        });
    }
    inner.append(&add_seg_btn);

    // ── Live preview graph ────────────────────────────────────────────────
    inner.append(
        &gtk::Label::builder()
            .label("Preview")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build(),
    );

    // Append the preview graph widget and wire name changes to update it.
    let graph_inner_widget = graph_for_update.widget().clone();
    graph_inner_widget.set_accessible_role(gtk::AccessibleRole::Img);
    graph_inner_widget
        .update_property(&[gtk::accessible::Property::Label("Workout preview graph")]);
    inner.append(&graph_inner_widget);

    {
        let draft_g = Rc::clone(&draft);
        let graph_g = Rc::clone(&graph_for_update);
        name_row.connect_changed(move |row| {
            let name = row.text().to_string();
            let w = build_draft_workout(&draft_g.borrow(), &name);
            graph_g.set_workout(&w);
        });
    }

    // ── Cancel / Save ─────────────────────────────────────────────────────
    let win_cancel = win.clone();
    cancel_btn.connect_clicked(move |_| win_cancel.close());

    {
        let draft_save = Rc::clone(&draft);
        let name_save = name_row.clone();
        let desc_save = desc_row.clone();
        let pool_save = pool;
        let rt_save = rt_handle;
        let workouts_save = workouts_list;
        let rebuild_save = rebuild;
        let on_toast_save = on_toast;
        let win_save = win.clone();
        // existing_id is Copy (Option<i64>), captured directly by the move closure

        save_btn.connect_clicked(move |_| {
            let name = name_save.text().trim().to_string();
            if name.is_empty() {
                on_toast_save(
                    adw::Toast::builder()
                        .title("Workout name is required")
                        .timeout(3)
                        .build(),
                );
                return;
            }
            let segs = draft_save.borrow().clone();
            if segs.is_empty() {
                on_toast_save(
                    adw::Toast::builder()
                        .title("Add at least one segment")
                        .timeout(3)
                        .build(),
                );
                return;
            }
            let desc = desc_save.text().trim().to_string();
            let mut workout = build_draft_workout(&segs, &name);
            workout.description = desc;

            // Clone per-invocation state for the async result handler (this closure is Fn).
            let pool_save = pool_save.clone();
            let workouts_save = Rc::clone(&workouts_save);
            let rebuild_save = rebuild_save.clone();
            let on_toast_save = Rc::clone(&on_toast_save);
            let win_save = win_save.clone();

            if let Some(id) = existing_id {
                workout.id = id;
                crate::ui::spawn_to_main(
                    &rt_save,
                    async move {
                        match db::update_workout(&pool_save, &workout).await {
                            Ok(()) => Ok(workout),
                            Err(e) => Err(e.to_string()),
                        }
                    },
                    move |res| match res {
                        Ok(workout) => {
                            let mut wl = workouts_save.borrow_mut();
                            if let Some(pos) = wl.iter().position(|w| w.id == id) {
                                wl[pos] = workout;
                            }
                            drop(wl);
                            rebuild_save();
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Workout updated")
                                    .timeout(3)
                                    .build(),
                            );
                            win_save.close();
                        }
                        Err(e) => {
                            tracing::error!("update_workout failed: {e}");
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Failed to update workout")
                                    .timeout(4)
                                    .build(),
                            );
                        }
                    },
                );
            } else {
                crate::ui::spawn_to_main(
                    &rt_save,
                    async move {
                        match db::save_workout(&pool_save, &workout).await {
                            Ok(id) => {
                                workout.id = id;
                                Ok(workout)
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    },
                    move |res| match res {
                        Ok(workout) => {
                            workouts_save.borrow_mut().push(workout);
                            rebuild_save();
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Workout saved")
                                    .timeout(3)
                                    .build(),
                            );
                            win_save.close();
                        }
                        Err(e) => {
                            tracing::error!("save_workout failed: {e}");
                            on_toast_save(
                                adw::Toast::builder()
                                    .title("Failed to save workout")
                                    .timeout(4)
                                    .build(),
                            );
                        }
                    },
                );
            }
        });
    }

    clamp.set_child(Some(&inner));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));
    win.set_content(Some(&toolbar_view));
    win.present();
}

/// Build a `Workout` from the editor's draft tuple list
/// `(duration_secs, power_low_pct, power_high_pct, label)`.
fn build_draft_workout(segs: &[(u32, f32, f32, String)], name: &str) -> Workout {
    use crate::data::workout::Segment;
    let segments: Vec<Segment> = segs
        .iter()
        .map(|(dur, lo, hi, lbl)| Segment {
            duration_secs: *dur,
            power_low_pct: *lo,
            power_high_pct: *hi,
            label: if lbl.is_empty() {
                None
            } else {
                Some(lbl.clone())
            },
            cadence_target: None,
        })
        .collect();
    Workout::from_segments(name, "", WorkoutCategory::Custom, segments)
}

/// The closure that redraws the editor's segment list.
///
/// Held in a self-reference (`holder`) so a row's delete button and the
/// drag-and-drop handler can trigger a redraw of the list they live in.
#[allow(clippy::too_many_arguments)]
fn segments_rebuild_closure(
    segments_group: adw::PreferencesGroup,
    draft: SegmentDraft,
    added_rows: Rc<RefCell<Vec<adw::ExpanderRow>>>,
    graph_rc: Rc<WorkoutGraph>,
    name_row: adw::EntryRow,
    rebuild_segs_holder: RebuildHolder,
) -> Rc<dyn Fn()> {
    Rc::new(move || {
        for row in added_rows.borrow().iter() {
            segments_group.remove(row);
        }
        added_rows.borrow_mut().clear();

        let segs = draft.borrow().clone();
        for (idx, (dur, lo, hi, label)) in segs.iter().enumerate() {
            let is_ramp = (hi - lo).abs() > 0.5;
            let title = if is_ramp {
                format!(
                    "{} min · {}→{}% FTP{}",
                    dur / 60,
                    lo.round() as u32,
                    hi.round() as u32,
                    if label.is_empty() {
                        String::new()
                    } else {
                        format!(" — {label}")
                    }
                )
            } else {
                format!(
                    "{} min · {}% FTP{}",
                    dur / 60,
                    lo.round() as u32,
                    if label.is_empty() {
                        String::new()
                    } else {
                        format!(" — {label}")
                    }
                )
            };

            let expander = adw::ExpanderRow::builder().title(&title).build();

            // Duration
            let dur_adj = gtk::Adjustment::new(*dur as f64, 10.0, 7200.0, 10.0, 60.0, 0.0);
            let dur_row = adw::SpinRow::new(Some(&dur_adj), 10.0, 0);
            dur_row.set_title("Duration");
            dur_row.set_subtitle("Seconds");
            expander.add_row(&dur_row);

            // Power low %
            let lo_adj = gtk::Adjustment::new(*lo as f64, 10.0, 200.0, 1.0, 5.0, 0.0);
            let lo_row = adw::SpinRow::new(Some(&lo_adj), 1.0, 0);
            lo_row.set_title("Power");
            lo_row.set_subtitle("% of FTP (start of segment)");
            expander.add_row(&lo_row);

            // Power high % (only relevant for ramps)
            let hi_adj = gtk::Adjustment::new(*hi as f64, 10.0, 200.0, 1.0, 5.0, 0.0);
            let hi_row = adw::SpinRow::new(Some(&hi_adj), 1.0, 0);
            hi_row.set_title("Power (end)");
            hi_row.set_subtitle("% of FTP (end of segment — set equal to start for steady)");
            expander.add_row(&hi_row);

            // Label
            let lbl_row = adw::EntryRow::builder().title("Label").build();
            lbl_row.set_text(label);
            expander.add_row(&lbl_row);

            // Delete button
            let del_btn = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove this segment")
                .css_classes(["destructive-action", "flat", "circular"])
                .valign(gtk::Align::Center)
                .build();
            expander.add_suffix(&del_btn);

            // Drag handle — lets the user grab and reorder the segment.
            let drag_handle = gtk::Image::builder()
                .icon_name("list-drag-handle-symbolic")
                .tooltip_text("Drag to reorder")
                .valign(gtk::Align::Center)
                .css_classes(["dim-label"])
                .build();
            expander.add_prefix(&drag_handle);

            // DragSource on the handle — broadcasts the source row index.
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gtk::gdk::DragAction::MOVE);
            let src_idx_capture = idx as u32;
            drag_source.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(
                    &src_idx_capture.to_value(),
                ))
            });
            drag_handle.add_controller(drag_source);

            // DropTarget on the expander row — moves the segment and rebuilds.
            let drop_target = gtk::DropTarget::new(u32::static_type(), gtk::gdk::DragAction::MOVE);
            {
                let draft_dnd = Rc::clone(&draft);
                let rebuild_dnd = Rc::clone(&rebuild_segs_holder);
                let target_idx = idx;
                drop_target.connect_drop(move |_, value, _, _| {
                    let src_idx = match value.get::<u32>() {
                        Ok(v) => v as usize,
                        Err(_) => return false,
                    };
                    if src_idx == target_idx {
                        return false;
                    }
                    let mut d = draft_dnd.borrow_mut();
                    if src_idx < d.len() {
                        let seg = d.remove(src_idx);
                        let insert_at = if src_idx < target_idx {
                            target_idx.saturating_sub(1)
                        } else {
                            target_idx
                        };
                        let cap = d.len();
                        d.insert(insert_at.min(cap), seg);
                        drop(d);
                        let rebuild = rebuild_dnd
                            .borrow()
                            .as_ref()
                            .expect("rebuild_segs set")
                            .clone();
                        rebuild();
                    }
                    true
                });
            }
            expander.add_controller(drop_target);

            // Wire spin/entry changes to update draft and preview graph
            {
                let draft_c = Rc::clone(&draft);
                let graph = Rc::clone(&graph_rc);
                let nr = name_row.clone();
                let i = idx;
                dur_row.connect_value_notify(move |row| {
                    if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                        seg.0 = row.value() as u32;
                    }
                    let name = nr.text().to_string();
                    let w = build_draft_workout(&draft_c.borrow(), &name);
                    graph.set_workout(&w);
                });
            }
            {
                let draft_c = Rc::clone(&draft);
                let graph = Rc::clone(&graph_rc);
                let nr = name_row.clone();
                let i = idx;
                lo_row.connect_value_notify(move |row| {
                    if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                        seg.1 = row.value() as f32;
                    }
                    let name = nr.text().to_string();
                    let w = build_draft_workout(&draft_c.borrow(), &name);
                    graph.set_workout(&w);
                });
            }
            {
                let draft_c = Rc::clone(&draft);
                let graph = Rc::clone(&graph_rc);
                let nr = name_row.clone();
                let i = idx;
                hi_row.connect_value_notify(move |row| {
                    if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                        seg.2 = row.value() as f32;
                    }
                    let name = nr.text().to_string();
                    let w = build_draft_workout(&draft_c.borrow(), &name);
                    graph.set_workout(&w);
                });
            }
            {
                let draft_c = Rc::clone(&draft);
                let i = idx;
                lbl_row.connect_changed(move |row| {
                    if let Some(seg) = draft_c.borrow_mut().get_mut(i) {
                        seg.3 = row.text().to_string();
                    }
                });
            }

            segments_group.add(&expander);
            added_rows.borrow_mut().push(expander);

            // Delete callback — removes the segment and triggers a full rebuild.
            let del_idx = idx;
            let draft_del = Rc::clone(&draft);
            let rebuild_del = Rc::clone(&rebuild_segs_holder);
            del_btn.connect_clicked(move |_| {
                let mut d = draft_del.borrow_mut();
                if d.len() > 1 {
                    d.remove(del_idx);
                    drop(d);
                    let rebuild = rebuild_del
                        .borrow()
                        .as_ref()
                        .expect("rebuild_segs set")
                        .clone();
                    rebuild();
                }
            });
        }

        // Push updated workout data into the graph (also queues a redraw).
        let name = name_row.text().to_string();
        let w = build_draft_workout(&segs, &name);
        graph_rc.set_workout(&w);
    })
}
