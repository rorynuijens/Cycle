//! The Coach card: AI interpretation of the numbers above it.
//!
//! Fitness analysis on top, retrospectives below behind a Week|Month switcher
//! (the same linked-toggle pattern as the calendar).

use adw::prelude::*;
use chrono::{Duration, Local, NaiveDate};
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::ai::coach::{build_fitness_prompt, get_suggestion, FitnessContext, WellnessSnapshot};
use crate::ai::context::parse_ai_sections;
use crate::ai::retrospective::{
    build_retrospective_prompt, RetroPeriod, RetroSession, RetrospectiveContext,
};
use crate::data::db::{self, WellnessEntry};
use crate::data::{athlete::AthleteProfile, keystore};
use crate::training::fitness::compute_load_metrics;
use crate::ui::markdown::to_pango;
use crate::ui::AiFailure;

use super::data::{
    load_fitness_prompt_data, load_retro_prompt_data, FitnessPromptData, RetroPromptData,
};

/// Shown when the rider has no Anthropic key configured.
const NO_API_KEY: &str = "No API key configured. Enter your Anthropic API key in \
                          Preferences → Integrations.";

/// Where each period's most recent retrospective is cached.
fn cache_key(period: RetroPeriod) -> &'static str {
    match period {
        RetroPeriod::Weekly => "ai.weekly_retrospective",
        RetroPeriod::Monthly => "ai.monthly_retrospective",
    }
}

/// How many days each retrospective looks back over.
fn period_days(period: RetroPeriod) -> i64 {
    match period {
        RetroPeriod::Weekly => 7,
        RetroPeriod::Monthly => 30,
    }
}

/// Turn database wellness rows into the shape the prompts take.
fn wellness_snapshots(entries: &[WellnessEntry]) -> Vec<WellnessSnapshot> {
    entries
        .iter()
        .map(|w| WellnessSnapshot {
            date: w.date.format("%Y-%m-%d").to_string(),
            hrv: w.hrv,
            resting_hr: w.resting_hr,
            sleep_hours: w.sleep_secs.map(|s| s as f32 / 3600.0),
            sleep_score: w.sleep_score,
            steps: w.steps,
            calories: w.calories,
        })
        .collect()
}

/// A block of AI output: structured sections when the model returns them,
/// a single formatted paragraph otherwise.
#[derive(Clone)]
struct AiOutput {
    container: gtk::Box,
    fallback: gtk::Label,
}

impl AiOutput {
    /// Build the output area. `placeholder` is what stands in before the first
    /// request, so the card explains itself rather than sitting blank.
    fn new(placeholder: &str, margin_y: i32) -> Self {
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();
        let fallback = gtk::Label::builder()
            .label(placeholder)
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .wrap(true)
            .selectable(true)
            .xalign(0.0)
            .margin_top(margin_y)
            .margin_bottom(margin_y)
            .margin_start(12)
            .margin_end(12)
            .build();
        container.append(&fallback);
        Self {
            container,
            fallback,
        }
    }

    /// Replace the output with `text`, splitting it into sections when the
    /// model returned numbered headings.
    fn set_text(&self, text: &str) {
        // Drop everything after the fallback label, which is always child 0.
        while let Some(child) = self.container.last_child() {
            if child == *self.fallback.upcast_ref::<gtk::Widget>() {
                break;
            }
            self.container.remove(&child);
        }

        let sections = parse_ai_sections(text);
        if sections.is_empty() {
            self.fallback.set_markup(&to_pango(text));
            self.fallback.remove_css_class("dim-label");
            self.fallback.set_visible(true);
            return;
        }

        self.fallback.set_visible(false);
        for (heading, body) in sections {
            self.container
                .append(&gtk::Separator::new(gtk::Orientation::Horizontal));

            let section = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(12)
                .margin_end(12)
                .build();

            let heading_label = gtk::Label::builder()
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading"])
                .build();
            heading_label.set_markup(&to_pango(&heading));
            section.append(&heading_label);

            let body_label = gtk::Label::builder()
                .halign(gtk::Align::Start)
                .wrap(true)
                .selectable(true)
                .xalign(0.0)
                .build();
            body_label.set_markup(&to_pango(&body));
            section.append(&body_label);

            self.container.append(&section);
        }
    }

    /// Show a plain status line — progress, or why a request produced nothing.
    fn set_status(&self, text: &str) {
        self.fallback.set_text(text);
        self.fallback.remove_css_class("dim-label");
    }
}

/// The Coach card.
pub struct CoachCard {
    root: gtk::Box,
    analysis: AiOutput,
}

impl CoachCard {
    /// `weekly_tss` reads the load-history bars at click time, so the prompt
    /// always carries what the page is currently showing.
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        weekly_tss: Rc<dyn Fn() -> Vec<f32>>,
    ) -> Self {
        let root = gtk::Box::builder()
            .css_classes(["card"])
            .orientation(gtk::Orientation::Vertical)
            .build();

        // ── Fitness analysis ──────────────────────────────────────────────
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        header.append(
            &gtk::Image::builder()
                .icon_name("chat-message-new-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        header.append(
            &gtk::Label::builder()
                .label("Coach")
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .tooltip_text(
                    "AI interpretation of your training metrics, recovery signals, \
                     and wellness data",
                )
                .build(),
        );
        let analyse_spinner = gtk::Spinner::new();
        analyse_spinner.set_visible(false);
        header.append(&analyse_spinner);

        let analyse_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Refresh AI fitness analysis")
            .valign(gtk::Align::Center)
            .build();
        header.append(&analyse_btn);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let analysis = AiOutput::new(
            "Select the refresh button above to get an AI-powered interpretation \
             of your training metrics, recovery signals, and wellness data.",
            12,
        );
        root.append(&analysis.container);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // ── Retrospectives ────────────────────────────────────────────────
        let retro_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        retro_header.append(
            &gtk::Label::builder()
                .label("Retrospective")
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .hexpand(true)
                .tooltip_text(
                    "AI analysis of strain, recovery patterns, and performance trends \
                     over the selected period",
                )
                .build(),
        );
        let retro_spinner = gtk::Spinner::new();
        retro_spinner.set_visible(false);
        retro_header.append(&retro_spinner);

        let week_toggle = gtk::ToggleButton::builder()
            .label("Week")
            .active(true)
            .build();
        let month_toggle = gtk::ToggleButton::builder().label("Month").build();
        month_toggle.set_group(Some(&week_toggle));
        let toggle_box = gtk::Box::builder()
            .css_classes(["linked"])
            .valign(gtk::Align::Center)
            .build();
        toggle_box.append(&week_toggle);
        toggle_box.append(&month_toggle);
        retro_header.append(&toggle_box);

        let generate_btn = gtk::Button::builder()
            .label("Generate")
            .css_classes(["pill"])
            .tooltip_text("Generate an AI retrospective for the selected period")
            .valign(gtk::Align::Center)
            .build();
        retro_header.append(&generate_btn);
        root.append(&retro_header);

        let weekly = AiOutput::new(
            "Select Generate for an AI retrospective of the past 7 days.",
            10,
        );
        let monthly = AiOutput::new(
            "Select Generate for an AI retrospective of the past 30 days.",
            10,
        );

        let retro_stack = gtk::Stack::builder()
            .vhomogeneous(false)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        retro_stack.add_named(&weekly.container, Some("week"));
        retro_stack.add_named(&monthly.container, Some("month"));
        root.append(&retro_stack);

        let stack = retro_stack.clone();
        week_toggle.connect_toggled(move |t| {
            if t.is_active() {
                stack.set_visible_child_name("week");
            }
        });
        let stack = retro_stack.clone();
        month_toggle.connect_toggled(move |t| {
            if t.is_active() {
                stack.set_visible_child_name("month");
            }
        });

        Self::restore_cached(&pool, &rt_handle, RetroPeriod::Weekly, &weekly);
        Self::restore_cached(&pool, &rt_handle, RetroPeriod::Monthly, &monthly);

        Self::connect_analyse(
            &analyse_btn,
            &analyse_spinner,
            &analysis,
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&athlete),
            weekly_tss,
        );
        Self::connect_generate(
            &generate_btn,
            &month_toggle,
            &retro_spinner,
            (weekly, monthly),
            pool,
            rt_handle,
            athlete,
        );

        Self { root, analysis }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Restore the fitness analysis cached from a previous session.
    pub fn set_cached_insight(&self, text: &str) {
        if !text.trim().is_empty() {
            self.analysis.set_text(text);
        }
    }

    /// Load one period's cached retrospective off the main thread (CLAUDE.md §2.3).
    fn restore_cached(
        pool: &SqlitePool,
        rt_handle: &tokio::runtime::Handle,
        period: RetroPeriod,
        output: &AiOutput,
    ) {
        let pool = pool.clone();
        let key = cache_key(period);
        let output = output.clone();
        crate::ui::spawn_to_main(
            rt_handle,
            async move {
                // A missing cache entry is normal; a failed read is not, but it
                // costs the rider nothing here — the card just shows its prompt
                // to generate one. Log it and carry on.
                db::get_setting(&pool, key)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!("Could not read cached retrospective: {e}");
                        None
                    })
                    .unwrap_or_default()
            },
            move |cached| {
                if !cached.is_empty() {
                    output.set_text(&cached);
                }
            },
        );
    }

    /// Write a finished answer back to the cache so it survives a restart.
    fn cache(pool: &SqlitePool, rt_handle: &tokio::runtime::Handle, key: &str, text: &str) {
        let pool = pool.clone();
        let key = key.to_string();
        let text = text.to_string();
        rt_handle.spawn(async move {
            if let Err(e) = db::set_setting(&pool, &key, &text).await {
                tracing::error!("Could not cache AI output: {e}");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_analyse(
        button: &gtk::Button,
        spinner: &gtk::Spinner,
        output: &AiOutput,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        weekly_tss: Rc<dyn Fn() -> Vec<f32>>,
    ) {
        let spinner = spinner.clone();
        let output = output.clone();
        button.connect_clicked(move |btn| {
            let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                Ok(Some(k)) if !k.trim().is_empty() => k,
                _ => {
                    output.set_status(NO_API_KEY);
                    return;
                }
            };

            // Read the !Send shared state on the main thread before spawning.
            let ftp_watts = athlete.borrow().ftp_watts;
            let week_tss = weekly_tss();
            let profile = athlete.borrow().clone();

            btn.set_sensitive(false);
            spinner.set_visible(true);
            spinner.start();
            output.set_status("Asking the AI Coach to analyse your fitness metrics…");

            let (tx, rx) = async_channel::bounded::<Result<String, AiFailure>>(1);
            let pool_task = pool.clone();
            // All DB reads + prompt assembly + the network call run off the main
            // thread, so the click never blocks the GLib loop (CLAUDE.md §2.3).
            rt_handle.spawn(async move {
                let FitnessPromptData {
                    records,
                    intervals_pairs,
                    icu_count,
                    wellness,
                    athlete_context,
                } = match load_fitness_prompt_data(&pool_task).await {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Could not read training history to analyse: {e}");
                        let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                        return;
                    }
                };

                let today = Local::now().date_naive();
                let metrics = compute_load_metrics(&records, &intervals_pairs, ftp_watts, today);
                let ctx = FitnessContext {
                    athlete: profile,
                    ctl: metrics.ctl,
                    atl: metrics.atl,
                    tsb: metrics.tsb(),
                    ctl_4wk_ago: metrics.ctl_4wk_ago,
                    week_tss,
                    total_sessions: records.len() + icu_count,
                    athlete_context,
                    wellness: wellness_snapshots(&wellness),
                };

                let result = get_suggestion(&api_key, &build_fitness_prompt(&ctx), 1400)
                    .await
                    .map_err(|e| {
                        tracing::error!("AI fitness analysis failed: {e}");
                        AiFailure::Request
                    });
                let _ = tx.send(result).await;
            });

            let btn = btn.clone();
            let spinner = spinner.clone();
            let output = output.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok(text) => {
                            output.set_text(&text);
                            Self::cache(&pool, &rt_handle, "ai.fitness_insight", &text);
                        }
                        Err(failure) => output.set_status(failure.message()),
                    }
                }
                spinner.stop();
                spinner.set_visible(false);
                btn.set_sensitive(true);
            });
        });
    }

    /// One Generate button serves both periods; dispatch on the active toggle.
    fn connect_generate(
        button: &gtk::Button,
        month_toggle: &gtk::ToggleButton,
        spinner: &gtk::Spinner,
        outputs: (AiOutput, AiOutput),
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
    ) {
        let (weekly, monthly) = outputs;
        let spinner = spinner.clone();
        let month_toggle = month_toggle.clone();
        button.connect_clicked(move |btn| {
            let period = if month_toggle.is_active() {
                RetroPeriod::Monthly
            } else {
                RetroPeriod::Weekly
            };
            let output = match period {
                RetroPeriod::Weekly => weekly.clone(),
                RetroPeriod::Monthly => monthly.clone(),
            };

            let api_key = match keystore::get_secret(keystore::KEY_ANTHROPIC) {
                Ok(Some(k)) if !k.trim().is_empty() => k,
                _ => {
                    output.set_status(NO_API_KEY);
                    return;
                }
            };

            let today = Local::now().date_naive();
            let days = period_days(period);
            let start_date = today - Duration::days(days - 1);
            let start_utc = start_date
                .and_hms_opt(0, 0, 0)
                .expect("midnight is always valid")
                .and_utc()
                .to_rfc3339();
            let end_utc = today
                .and_hms_opt(23, 59, 59)
                .expect("end of day is always valid")
                .and_utc()
                .to_rfc3339();

            // Read the !Send shared state on the main thread before spawning.
            let ftp_watts = athlete.borrow().ftp_watts;
            let profile = athlete.borrow().clone();

            btn.set_sensitive(false);
            spinner.set_visible(true);
            spinner.start();
            output.set_status(&format!(
                "Generating {} retrospective analysis…",
                period.label()
            ));

            let (tx, rx) = async_channel::bounded::<Result<String, AiFailure>>(1);
            let pool_task = pool.clone();
            // All DB reads + prompt assembly + the network call run off the main
            // thread, so the click never blocks the GLib loop (CLAUDE.md §2.3).
            rt_handle.spawn(async move {
                let RetroPromptData {
                    records,
                    icu_acts,
                    intervals_all,
                    wellness,
                    all_records,
                    athlete_context,
                } = match load_retro_prompt_data(
                    &pool_task, &start_utc, &end_utc, start_date, today,
                )
                .await
                {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Could not read training history for retrospective: {e}");
                        let _ = tx.send(Err(AiFailure::DataUnavailable)).await;
                        return;
                    }
                };

                let all_rides: Vec<db::SessionSummary> =
                    all_records.iter().map(|r| r.summary()).collect();
                let end = compute_load_metrics(&all_rides, &intervals_all, ftp_watts, today);
                // Fitness at the start of the period, so the prompt can say
                // which way the trend went rather than just where it landed.
                let ctl_start = compute_load_metrics(
                    &all_rides,
                    &intervals_all,
                    ftp_watts,
                    today - Duration::days(days),
                )
                .ctl;

                let ctx = RetrospectiveContext {
                    athlete: profile,
                    period,
                    sessions: retro_sessions(&records, &icu_acts, ftp_watts, start_date, today),
                    wellness: wellness_snapshots(&wellness),
                    ctl_start,
                    ctl_end: end.ctl,
                    atl_end: end.atl,
                    tsb_end: end.tsb(),
                    athlete_context,
                };

                let result = get_suggestion(&api_key, &build_retrospective_prompt(&ctx), 2048)
                    .await
                    .map_err(|e| {
                        tracing::error!("AI retrospective failed: {e}");
                        AiFailure::Request
                    });
                let _ = tx.send(result).await;
            });

            let btn = btn.clone();
            let spinner = spinner.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok(text) => {
                            output.set_text(&text);
                            Self::cache(&pool, &rt_handle, cache_key(period), &text);
                        }
                        Err(failure) => output.set_status(failure.message()),
                    }
                }
                spinner.stop();
                spinner.set_visible(false);
                btn.set_sensitive(true);
            });
        });
    }
}

/// Every activity inside the retrospective period, in date order.
///
/// In-app sessions and unlinked Intervals.icu activities are merged; power and
/// work only carry meaning for cycling, so other sports leave them empty rather
/// than reporting zeros the AI would read as a very easy ride.
fn retro_sessions(
    records: &[db::SessionRecord],
    icu_acts: &[db::IntervalsActivity],
    ftp_watts: u32,
    start_date: NaiveDate,
    today: NaiveDate,
) -> Vec<RetroSession> {
    let mut sessions: Vec<RetroSession> = records
        .iter()
        .map(|r| RetroSession {
            date: r
                .session
                .started_at
                .with_timezone(&Local)
                .date_naive()
                .format("%Y-%m-%d")
                .to_string(),
            name: r.workout_name.clone(),
            sport_type: "Cycling".to_string(),
            duration_mins: r.session.duration_secs() as u32 / 60,
            avg_power: r.session.average_power().map(|p| p as u32),
            tss: r.session.tss(ftp_watts),
            kj: r.session.kilojoules(),
        })
        .collect();

    for act in icu_acts {
        if act.date < start_date || act.date > today {
            continue;
        }
        let sport = crate::data::sport::normalize_sport_type(&act.sport_type);
        let is_cycling = sport == "Cycling";
        sessions.push(RetroSession {
            date: act.date.format("%Y-%m-%d").to_string(),
            name: if act.name.is_empty() {
                None
            } else {
                Some(act.name.clone())
            },
            sport_type: sport,
            duration_mins: act.duration_secs.unwrap_or(0) / 60,
            avg_power: if is_cycling { act.average_watts } else { None },
            tss: act.tss,
            kj: if is_cycling {
                act.average_watts
                    .and_then(|w| act.duration_secs.map(|d| w as f32 * d as f32 / 1000.0))
                    .unwrap_or(0.0)
            } else {
                0.0
            },
        });
    }

    sessions.sort_by(|a, b| a.date.cmp(&b.date));
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_cache_each_period_separately() {
        assert_ne!(
            cache_key(RetroPeriod::Weekly),
            cache_key(RetroPeriod::Monthly)
        );
    }

    #[test]
    fn should_look_back_a_week_and_a_month() {
        assert_eq!(period_days(RetroPeriod::Weekly), 7);
        assert_eq!(period_days(RetroPeriod::Monthly), 30);
    }

    #[test]
    fn should_carry_every_wellness_field_into_the_prompt() {
        let entry = WellnessEntry {
            date: NaiveDate::from_ymd_opt(2026, 8, 7).expect("hardcoded valid date"),
            hrv: Some(58.0),
            resting_hr: Some(46),
            sleep_secs: Some(27_000),
            sleep_score: Some(84),
            steps: Some(9_400),
            calories: Some(2_650),
        };
        let snapshots = wellness_snapshots(&[entry]);
        let snapshot = snapshots.first().expect("one entry in, one out");
        assert_eq!(snapshot.date, "2026-08-07");
        assert_eq!(snapshot.hrv, Some(58.0));
        assert_eq!(snapshot.resting_hr, Some(46));
        assert_eq!(snapshot.sleep_hours, Some(7.5));
        assert_eq!(snapshot.sleep_score, Some(84));
        assert_eq!(snapshot.steps, Some(9_400));
        assert_eq!(snapshot.calories, Some(2_650));
    }

    #[test]
    fn should_send_no_wellness_when_nothing_is_synced() {
        assert!(wellness_snapshots(&[]).is_empty());
    }
}
