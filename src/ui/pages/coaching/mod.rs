//! The Coaching page: what to ride today, what to aim at, and a plan to get there.
//!
//! Three independent sections stacked in a clamp. Only the goals list and the
//! cached suggestion come from the database on reload; everything else is driven
//! by the rider pressing a button.

pub(crate) mod data;
mod goals;
mod plan;
mod program;
mod suggestion;

use adw::prelude::*;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, db, workout::Workout};
use crate::ui::widgets::api_key_banner::ApiKeyBanner;

use data::{load_coaching_data, CoachingData};
use goals::GoalsSection;
use plan::PlanCard;
use program::ProgramSection;
use suggestion::SuggestionCard;

pub struct CoachingPage {
    root: gtk::Box,
}

impl CoachingPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` when the page becomes
    /// visible or after a goal changes.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        workouts: Vec<Workout>,
        on_start_workout: Rc<dyn Fn(Workout)>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        brief_store: Rc<crate::ui::brief_store::BriefStore>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let api_banner = ApiKeyBanner::new(
            "Add your AI provider API key in Preferences → Integrations to use AI features",
        );
        root.append(api_banner.widget());

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

        let workouts = Rc::new(workouts);

        let suggestion = Rc::new(SuggestionCard::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete),
            Rc::clone(&workouts),
            on_start_workout,
            Rc::clone(&on_toast),
            Rc::clone(&brief_store),
        ));
        let goals = Rc::new(GoalsSection::new(pool.clone(), rt_handle.clone()));
        let plan = PlanCard::new(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete),
            Rc::clone(&workouts),
            Rc::clone(&on_toast),
        );

        // The reconciliation: the brief's verdict reaches the plan, which is
        // the only thing that produces an adjustment. Re-run the rules whenever
        // the verdict changes, so easing follows the brief without the brief
        // ever picking a session — see training::program::suggest.
        brief_store.observe({
            let plan = Rc::clone(&plan);
            move |state: &crate::ui::brief_store::BriefState| {
                let verdict = state.brief.as_ref().map(|b| b.verdict).unwrap_or_default();
                plan.set_verdict(verdict);
            }
        });
        let program = ProgramSection::new(
            pool.clone(),
            rt_handle.clone(),
            athlete,
            workouts,
            on_toast.clone(),
        );

        // The rider's Intervals.icu templates, refreshed on each page load. The
        // brief can arrive before or after them, so both paths render through
        // whatever is currently here rather than assuming an order.
        let icu_workouts: Rc<RefCell<Rc<Vec<db::IntervalsWorkout>>>> =
            Rc::new(RefCell::new(Rc::new(vec![])));

        brief_store.observe({
            let suggestion = Rc::clone(&suggestion);
            let icu_workouts = Rc::clone(&icu_workouts);
            move |state: &crate::ui::brief_store::BriefState| {
                let icu = Rc::clone(&icu_workouts.borrow());
                suggestion.apply_brief(state, &icu);
            }
        });

        inner.append(suggestion.widget());
        // The plan the rider is living with comes before the builder that makes
        // a new one: following one is the common case, starting one is not.
        inner.append(plan.widget());
        inner.append(goals.widget());
        inner.append(program.widget());

        clamp.set_child(Some(&inner));
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        root.append(&scroll);

        let reload: Rc<dyn Fn()> = {
            let pool = pool.clone();
            let goals = Rc::clone(&goals);
            let on_toast = Rc::clone(&on_toast);
            let plan = Rc::clone(&plan);
            let icu_for_reload = Rc::clone(&icu_workouts);
            let brief_for_reload = Rc::clone(&brief_store);
            Rc::new(move || {
                api_banner.refresh();
                // The program's state is read on every visit: a ride finished
                // since the page was last open changes what it has to say.
                plan.reload();

                // Load off the main thread (CLAUDE.md §2.3), then fill the goals
                // list and restore the last suggestion once it arrives.
                let pool_load = pool.clone();
                let goals = Rc::clone(&goals);
                let suggestion = Rc::clone(&suggestion);
                let on_toast = Rc::clone(&on_toast);
                let icu_for_reload = Rc::clone(&icu_for_reload);
                let brief_for_reload = Rc::clone(&brief_for_reload);
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { load_coaching_data(&pool_load).await },
                    move |result| match result {
                        Ok(CoachingData {
                            goals: entries,
                            icu_workouts,
                        }) => {
                            goals.set_goals(&entries);
                            // The brief may already have arrived; re-render it
                            // now that the Intervals.icu names are on hand.
                            *icu_for_reload.borrow_mut() = Rc::new(icu_workouts);
                            let icu = Rc::clone(&icu_for_reload.borrow());
                            suggestion.apply_brief(&brief_for_reload.state(), &icu);
                        }
                        // A failed load must not redraw the page as empty: an
                        // empty goals list reads as "you have no goals", which
                        // is a statement about the rider, not the database.
                        Err(e) => {
                            tracing::error!("Could not load coaching data: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title("Could not load your goals")
                                    .timeout(5)
                                    .build(),
                            );
                        }
                    },
                );
            })
        };

        // Adding or removing a goal reloads the page it came from.
        goals.set_reload(Rc::clone(&reload));
        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}
