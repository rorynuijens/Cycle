//! The workout list: which workouts show, and what each row says about them.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::data::athlete::AthleteProfile;
use crate::data::db;
use crate::data::workout::{Workout, WorkoutCategory};
use crate::training::recommend::workout_fit;
use crate::ui::widgets::workout_graph::WorkoutGraph;

use super::detail::show_workout_detail;
use super::editor::show_workout_editor;
use super::{FitnessContext, RebuildHolder};

/// The order categories appear in, easiest first.
pub const CATEGORY_ORDER: [WorkoutCategory; 8] = [
    WorkoutCategory::Recovery,
    WorkoutCategory::Endurance,
    WorkoutCategory::Tempo,
    WorkoutCategory::SweetSpot,
    WorkoutCategory::Threshold,
    WorkoutCategory::Vo2Max,
    WorkoutCategory::Anaerobic,
    WorkoutCategory::Custom,
];

/// How hard a workout feels.
///
/// Keyed on peak segment intensity (% FTP) rather than TSS, because TSS
/// undervalues short high-intensity work: a 6×15 s sprint session at 175 % FTP
/// scores very little TSS but is physiologically very hard. TSS still raises
/// the verdict for long steady work, where intensity alone would understate it.
pub fn difficulty(workout: &Workout) -> &'static str {
    let peak_pct = workout
        .segments
        .iter()
        .map(|s| s.power_high_pct.max(s.power_low_pct))
        .fold(0.0f32, f32::max);
    let tss = workout.tss as u32;

    if peak_pct >= 130.0 {
        "Very Hard"
    } else if peak_pct >= 110.0 || tss > 100 {
        "Hard"
    } else if peak_pct >= 88.0 || tss > 50 {
        "Moderate"
    } else {
        "Easy"
    }
}

/// The line under a workout's name.
///
/// The category is already the section heading, so it carries difficulty
/// instead, and the workout's own description when it has one.
pub fn row_subtitle(workout: &Workout) -> String {
    let meta = format!(
        "{} min · TSS {} · {}",
        workout.duration_secs / 60,
        workout.tss as u32,
        difficulty(workout)
    );
    let description = workout.description.trim();
    if description.is_empty() {
        meta
    } else {
        format!("{meta} — {description}")
    }
}

/// Does this workout survive the current filters?
///
/// No categories selected means no category filter, not "none of them".
pub fn matches(
    workout: &Workout,
    category: WorkoutCategory,
    active: &HashSet<WorkoutCategory>,
    search_lower: &str,
) -> bool {
    if workout.category != category {
        return false;
    }
    if !active.is_empty() && !active.contains(&category) {
        return false;
    }
    search_lower.is_empty() || workout.name.to_lowercase().contains(search_lower)
}

/// The shared handles a row's buttons need.
pub struct RowContext {
    pub pool: SqlitePool,
    pub rt_handle: tokio::runtime::Handle,
    pub on_start: Rc<dyn Fn(Workout)>,
    pub on_toast: Rc<dyn Fn(adw::Toast)>,
    pub workouts: Rc<RefCell<Vec<Workout>>>,
    pub rebuild: RebuildHolder,
    pub athlete: Rc<RefCell<AthleteProfile>>,
    pub calendar_icon: &'static str,
}

impl RowContext {
    /// The rebuild closure, which is only set after the list is first built.
    fn rebuild_fn(&self) -> Option<Rc<dyn Fn()>> {
        self.rebuild.borrow().clone()
    }
}

/// Build one workout's row: thumbnail, meta, recommendation star, and — for
/// custom workouts — the edit and delete buttons.
pub fn build_row(
    workout: &Workout,
    ftp: u32,
    fitness: &FitnessContext,
    ctx: &Rc<RowContext>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&workout.name)
        .subtitle(row_subtitle(workout))
        .activatable(true)
        .build();

    // The workout's own shape is its best label: a mini zone-coloured profile,
    // the same motif as the player and summary.
    let thumb = WorkoutGraph::new(workout, ftp);
    thumb.widget().set_content_width(84);
    thumb.widget().set_content_height(42);
    thumb.widget().set_hexpand(false);
    thumb.widget().set_valign(gtk::Align::Center);
    row.add_prefix(thumb.widget());

    if workout_fit(workout, fitness.ctl, fitness.tsb, &fitness.goals).recommended {
        row.add_suffix(
            &gtk::Image::builder()
                .icon_name("starred-symbolic")
                .css_classes(["success"])
                .tooltip_text("Recommended based on your current fitness and goals")
                .valign(gtk::Align::Center)
                .build(),
        );
    }

    {
        let ctx = Rc::clone(ctx);
        let workout = workout.clone();
        let (ctl, tsb, goals) = (fitness.ctl, fitness.tsb, Rc::clone(&fitness.goals));
        row.connect_activated(move |row| {
            let parent = row.root().and_downcast::<gtk::Window>();
            show_workout_detail(
                workout.clone(),
                ftp,
                ctl,
                tsb,
                Rc::clone(&goals),
                Rc::clone(&ctx.on_start),
                Rc::clone(&ctx.on_toast),
                ctx.pool.clone(),
                ctx.rt_handle.clone(),
                ctx.calendar_icon,
                parent.as_ref(),
            );
        });
    }

    // Only workouts the rider made can be edited or deleted.
    if workout.category == WorkoutCategory::Custom {
        row.add_suffix(&edit_button(workout, ftp, ctx));
        row.add_suffix(&delete_button(workout.id, ctx));
    }

    row
}

fn edit_button(workout: &Workout, ftp: u32, ctx: &Rc<RowContext>) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .tooltip_text("Edit this workout")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();

    let ctx = Rc::clone(ctx);
    let workout = workout.clone();
    button.connect_clicked(move |btn| {
        let Some(rebuild) = ctx.rebuild_fn() else {
            return;
        };
        let parent = btn.root().and_downcast::<gtk::Window>();
        show_workout_editor(
            parent.as_ref(),
            ctx.pool.clone(),
            ctx.rt_handle.clone(),
            ftp,
            Rc::clone(&ctx.workouts),
            rebuild,
            Rc::clone(&ctx.on_toast),
            Some(workout.clone()),
        );
    });

    button
}

fn delete_button(workout_id: i64, ctx: &Rc<RowContext>) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete this workout")
        .css_classes(["destructive-action", "flat", "circular"])
        .valign(gtk::Align::Center)
        .build();

    let ctx = Rc::clone(ctx);
    button.connect_clicked(move |btn| {
        let ctx = Rc::clone(&ctx);
        crate::ui::widgets::dialog::confirm_destructive(
            btn,
            "Delete Workout?",
            "This workout will be permanently removed.",
            "_Delete",
            move || {
                let ctx = Rc::clone(&ctx);
                let pool = ctx.pool.clone();
                crate::ui::spawn_to_main(
                    &ctx.rt_handle.clone(),
                    async move { db::delete_workout(&pool, workout_id).await },
                    move |res| {
                        if let Err(e) = res {
                            tracing::error!("delete_workout failed: {e}");
                            (ctx.on_toast)(
                                adw::Toast::builder()
                                    .title("Failed to delete workout")
                                    .timeout(4)
                                    .build(),
                            );
                            return;
                        }
                        ctx.workouts.borrow_mut().retain(|w| w.id != workout_id);
                        if let Some(rebuild) = ctx.rebuild_fn() {
                            rebuild();
                        }
                    },
                );
            },
        );
    });

    button
}

/// The state page shown when the filters exclude everything, offering the way
/// out of it.
pub fn empty_state(
    filter_chips: Rc<RefCell<Vec<gtk::ToggleButton>>>,
    search_entry: gtk::SearchEntry,
) -> adw::StatusPage {
    let clear_btn = gtk::Button::builder()
        .label("Clear Filters")
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .tooltip_text("Show all workouts again")
        .build();
    clear_btn.connect_clicked(move |_| {
        for chip in filter_chips.borrow().iter() {
            chip.set_active(false);
        }
        search_entry.set_text("");
    });

    adw::StatusPage::builder()
        .icon_name("folder-open-symbolic")
        .title("No Workouts")
        .description("No workouts match your search or filters.")
        .child(&clear_btn)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;

    fn workout(tss: f32, peak_pct: f32, duration_secs: u32) -> Workout {
        Workout {
            tss,
            duration_secs,
            description: String::new(),
            segments: vec![Segment {
                duration_secs,
                power_low_pct: peak_pct,
                power_high_pct: peak_pct,
                label: None,
                cadence_target: None,
            }],
            ..Workout::sample_threshold()
        }
    }

    #[test]
    fn should_call_a_sprint_session_very_hard_despite_low_tss() {
        // The reason difficulty is not keyed on TSS: 6×15 s at 175 % FTP scores
        // almost nothing but is among the hardest things in the library.
        let sprints = workout(12.0, 175.0, 900);
        assert_eq!(difficulty(&sprints), "Very Hard");
    }

    #[test]
    fn should_call_a_long_steady_ride_hard_on_tss_alone() {
        // Three hours at 70 % FTP never gets near the intensity thresholds, but
        // it is not an easy afternoon.
        let endurance = workout(180.0, 70.0, 10_800);
        assert_eq!(difficulty(&endurance), "Hard");
    }

    #[test]
    fn should_call_a_recovery_spin_easy() {
        assert_eq!(difficulty(&workout(20.0, 50.0, 1800)), "Easy");
    }

    #[test]
    fn should_grade_on_the_hardest_segment_not_the_average() {
        // A warmup at 50 % followed by one VO2 effort is a hard workout.
        let mut mixed = workout(45.0, 50.0, 3600);
        mixed.segments.push(Segment {
            duration_secs: 300,
            power_low_pct: 118.0,
            power_high_pct: 118.0,
            label: None,
            cadence_target: None,
        });
        assert_eq!(difficulty(&mixed), "Hard");
    }

    #[test]
    fn should_grade_a_workout_with_no_segments_without_panicking() {
        let mut empty = workout(0.0, 0.0, 0);
        empty.segments.clear();
        assert_eq!(difficulty(&empty), "Easy");
    }

    #[test]
    fn should_put_duration_stress_and_difficulty_in_the_subtitle() {
        let subtitle = row_subtitle(&workout(75.0, 95.0, 3600));
        assert!(subtitle.contains("60 min"), "got {subtitle}");
        assert!(subtitle.contains("TSS 75"), "got {subtitle}");
        assert!(subtitle.contains("Moderate"), "got {subtitle}");
    }

    #[test]
    fn should_append_a_description_when_there_is_one() {
        let mut w = workout(75.0, 95.0, 3600);
        w.description = "  Over-unders at threshold  ".into();
        assert!(row_subtitle(&w).ends_with("— Over-unders at threshold"));
    }

    #[test]
    fn should_leave_the_subtitle_alone_when_the_description_is_blank() {
        let mut w = workout(75.0, 95.0, 3600);
        w.description = "   ".into();
        assert!(!row_subtitle(&w).contains('—'));
    }

    #[test]
    fn should_show_every_category_when_none_is_selected() {
        // An empty chip set means "no filter", not "exclude everything".
        let w = workout(50.0, 90.0, 3600);
        let none = HashSet::new();
        assert!(matches(&w, w.category, &none, ""));
    }

    #[test]
    fn should_hide_categories_that_are_not_selected() {
        let w = workout(50.0, 90.0, 3600);
        let mut active = HashSet::new();
        active.insert(WorkoutCategory::Recovery);
        assert!(!matches(&w, w.category, &active, ""));
    }

    #[test]
    fn should_match_a_search_regardless_of_case() {
        let mut w = workout(50.0, 90.0, 3600);
        w.name = "Sweet Spot Builder".into();
        let none = HashSet::new();
        assert!(matches(&w, w.category, &none, "sweet"));
        assert!(matches(&w, w.category, &none, "builder"));
        assert!(!matches(&w, w.category, &none, "sprint"));
    }

    #[test]
    fn should_list_every_category_in_the_filter_order() {
        // A category missing here would never appear in the list at all.
        assert_eq!(CATEGORY_ORDER.len(), 8);
        for cat in CATEGORY_ORDER {
            assert!(CATEGORY_ORDER.iter().filter(|c| **c == cat).count() == 1);
        }
    }
}
