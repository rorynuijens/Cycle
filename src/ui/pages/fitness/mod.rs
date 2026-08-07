//! The Fitness page: how fit the rider is, how tired, and what the trend says.
//!
//! The page is a stack of independent sections, each owning its own widgets and
//! exposing a small setter. [`FitnessPage::new`] wires them together; one reload
//! loads the database in a single pass and hands each section its slice.

mod bests;
mod coach;
mod data;
mod form_hero;
mod load_history;
mod wellness;
mod zones;

use adw::prelude::*;
use chrono::{Duration, Local};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, keystore};
use crate::training::analytics::{
    compute_hr_zones, compute_pace_curve, compute_power_curve, compute_volume_totals,
    compute_weekly_tss, compute_zone_seconds, RECENT_WINDOW_DAYS,
};
use crate::training::fitness::compute_pmc_series;

use bests::BestsSection;
use coach::CoachCard;
use data::{load_fitness_data, FitnessData};
use form_hero::FormHero;
use load_history::{LoadHistory, TSS_WEEKS};
use wellness::WellnessGrid;
use zones::ZonesSection;

/// How far back the performance-management chart plots.
const PMC_WINDOW_DAYS: i64 = 90;

/// The page's sections, in the order they appear.
///
/// The page reads: form → wellness → zones → bests → load → coach.
struct Sections {
    hero: FormHero,
    wellness: WellnessGrid,
    zones: ZonesSection,
    bests: BestsSection,
    load: Rc<LoadHistory>,
    coach: CoachCard,
}

impl Sections {
    /// Hand every section its slice of one database load.
    ///
    /// This page needs the samples themselves for the zone distribution and the
    /// power curve, so it loads full records and reduces them here rather than
    /// querying twice.
    fn apply(&self, data: FitnessData, athlete: &AthleteProfile) {
        let FitnessData {
            records,
            intervals_pairs,
            icu_activities,
            wellness,
            run_streams,
            cached_insight,
        } = data;

        let today = Local::now().date_naive();
        let ftp_watts = athlete.ftp_watts;
        let rides: Vec<_> = records.iter().map(|r| r.summary()).collect();

        self.hero.set_synced_count(icu_activities.len());
        let metrics = crate::training::fitness::compute_load_metrics(
            &rides,
            &intervals_pairs,
            ftp_watts,
            today,
        );
        self.hero.set_form(metrics.ctl, metrics.atl, metrics.tsb());
        self.hero.set_pmc_series(compute_pmc_series(
            &rides,
            &intervals_pairs,
            ftp_watts,
            today,
            PMC_WINDOW_DAYS,
        ));

        self.wellness.set_entries(&wellness, today);

        self.load
            .set_volume(&compute_volume_totals(&records, &icu_activities, today));
        self.load.set_weekly_tss(&compute_weekly_tss(
            &records,
            &intervals_pairs,
            ftp_watts,
            today,
            TSS_WEEKS,
        ));

        self.zones.set_zones(
            &compute_zone_seconds(&records, ftp_watts),
            &compute_hr_zones(&records, athlete.max_hr),
        );

        let recent_cutoff = today - Duration::days(RECENT_WINDOW_DAYS);
        self.bests.set_curves(
            compute_power_curve(&records, recent_cutoff),
            compute_pace_curve(&run_streams, recent_cutoff),
        );

        self.coach.set_cached_insight(&cached_insight);
    }
}

pub struct FitnessPage {
    root: gtk::Box,
}

impl FitnessPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` when the page becomes visible
    /// or after the athlete's FTP changes.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) -> (Self, Rc<dyn Fn()>) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let api_banner = adw::Banner::builder()
            .title("Add your Anthropic API key in Preferences → Integrations to use AI features")
            .button_label("Open Preferences")
            .revealed(false)
            .build();
        // The banner only ever shows to someone with no key yet, so its button
        // has to take them somewhere. `app.preferences` is the same action the
        // main menu uses.
        api_banner.connect_button_clicked(|banner| {
            if let Err(e) = banner.activate_action("app.preferences", None) {
                tracing::error!("Could not open Preferences from the API key banner: {e}");
            }
        });
        root.append(&api_banner);

        // Chart-dominated page — wider clamp than the standard 900 (same
        // justification as the calendar) so the PMC and curves get usable
        // horizontal resolution.
        let clamp = adw::Clamp::builder()
            .maximum_size(1200)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();

        // The load history feeds the AI prompt, so it is built first and read
        // back at click time — the prompt then carries what the page is showing.
        let load = Rc::new(LoadHistory::new());
        let weekly_tss: Rc<dyn Fn() -> Vec<f32>> = {
            let load = Rc::clone(&load);
            Rc::new(move || load.weekly_tss())
        };

        let sections = Rc::new(Sections {
            hero: FormHero::new(),
            wellness: WellnessGrid::new(),
            zones: ZonesSection::new(),
            bests: BestsSection::new(Rc::clone(&athlete)),
            coach: CoachCard::new(
                pool.clone(),
                rt_handle.clone(),
                Rc::clone(&athlete),
                weekly_tss,
            ),
            load,
        });

        inner.append(sections.hero.widget());
        inner.append(sections.wellness.widget());
        inner.append(sections.zones.widget());
        inner.append(sections.bests.widget());
        inner.append(sections.load.widget());
        inner.append(sections.coach.widget());

        clamp.set_child(Some(&inner));
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        root.append(&scroll);

        let reload: Rc<dyn Fn()> = {
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let athlete = Rc::clone(&athlete);
            let sections = Rc::clone(&sections);
            let api_banner = api_banner.clone();
            let on_toast = Rc::clone(&on_toast);
            Rc::new(move || {
                // API key pre-flight check (local keyring — fast, stays synchronous)
                let has_api_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
                    .unwrap_or(None)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false);
                api_banner.set_revealed(!has_api_key);

                // Load every data source off the main thread (CLAUDE.md §2.3),
                // then hand the sections their slices once it arrives.
                let pool_load = pool.clone();
                let sections = Rc::clone(&sections);
                let athlete = Rc::clone(&athlete);
                let on_toast = Rc::clone(&on_toast);
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { load_fitness_data(&pool_load).await },
                    move |result| match result {
                        Ok(data) => sections.apply(data, &athlete.borrow()),
                        // A failed load must not redraw the page as empty — an
                        // empty chart is indistinguishable from "you have never
                        // ridden". Say so instead, and leave the last good view up.
                        Err(e) => {
                            tracing::error!("Could not load fitness data: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title("Could not load your fitness data")
                                    .timeout(5)
                                    .build(),
                            );
                        }
                    },
                );
            })
        };

        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}
