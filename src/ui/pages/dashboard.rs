use adw::prelude::*;
use chrono::{Duration, Local, Timelike};
use std::cell::RefCell;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::{
    ai_cache,
    athlete::AthleteProfile,
    db::{self},
    settings,
    workout::Workout,
};
use crate::training::fitness::compute_load_metrics;
use crate::training::program::CoachVerdict;
use crate::ui::brief_store::{BriefState, BriefStatus, BriefStore};
use crate::ui::markdown::{insight_preview, to_pango};
use crate::ui::widgets::api_key_banner::ApiKeyBanner;
use crate::ui::widgets::workout_graph::WorkoutGraph;
use crate::ui::AiFailure;

use crate::ui::ReloadHolder;

/// Everything the dashboard's cards are drawn from, loaded in one pass.
struct DashboardData {
    today_entry: Option<db::TodayEntry>,
    records: Vec<db::SessionSummary>,
    /// The brief's recommendation resolved against the library, when today is
    /// open and no program owned the day.
    suggested_workout: Option<Workout>,
    ai_workout_detail: String,
    ai_fitness_insight: String,
    intervals_pairs: Vec<(chrono::NaiveDate, f32)>,
    first_use_done: bool,
}

/// Load the dashboard's data off the GTK main thread (CLAUDE.md §2.3).
///
/// The hero and the form row read today's brief from the cache rather than
/// being handed it, so a reload triggered by anything — a finished ride, an FTP
/// edit — redraws them from the same brief the Coach card above is showing.
async fn load_dashboard_data(pool: &SqlitePool, today: &str) -> anyhow::Result<DashboardData> {
    let today_entry = db::load_today_entry(pool, today).await?;
    let brief = ai_cache::daily_brief(pool)
        .await?
        .filter(|b| b.is_for(today));

    // A recommendation only matters on an empty day — with a workout scheduled,
    // the plan wins and the suggestion hides.
    let recommended = brief
        .as_ref()
        .and_then(|b| b.recommended_workout.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty());

    let suggested_workout = match (today_entry.is_none(), recommended) {
        (true, Some(name)) => db::load_workouts(pool)
            .await?
            .into_iter()
            // Case-insensitive — the AI may return different casing.
            .find(|w| crate::ai::naming::names_match(&w.name, name)),
        _ => None,
    };

    let ai_workout_detail = suggested_workout
        .as_ref()
        .map(|w| {
            format!(
                "{} · {} min · TSS {:.0}",
                w.category.label(),
                w.duration_secs / 60,
                w.tss
            )
        })
        .unwrap_or_default();

    Ok(DashboardData {
        today_entry,
        records: db::load_session_summaries(pool).await?,
        suggested_workout,
        ai_workout_detail,
        ai_fitness_insight: brief
            .as_ref()
            .and_then(|b| b.form_slice())
            .unwrap_or_default()
            .to_string(),
        intervals_pairs: db::load_intervals_tss_pairs(pool).await?,
        first_use_done: settings::first_use_complete(pool).await?,
    })
}

/// Put `name` on today's calendar in place of whatever was there.
///
/// Used when the rider accepts the coach's alternative suggestion.
async fn swap_todays_workout(pool: &SqlitePool, name: &str, today: &str) -> anyhow::Result<()> {
    let workouts = db::load_workouts(pool).await?;
    // Case-insensitive match — the AI may return a different casing.
    let alt = workouts
        .iter()
        .find(|w| crate::ai::naming::names_match(&w.name, name))
        .ok_or_else(|| anyhow::anyhow!("workout '{name}' is not in the library"))?;

    // Clear the day first so it reflects what was actually chosen.
    if let Some(entry) = db::load_today_entry(pool, today).await? {
        db::delete_today_calendar_entry(pool, entry.workout.id, today).await?;
    }
    db::schedule_workout(pool, alt.id, today, None).await?;
    Ok(())
}

pub struct DashboardPage {
    root: gtk::Box,
}

impl DashboardPage {
    /// Returns `(page, reload_fn)`. Call `reload_fn()` whenever data may have changed.
    #[allow(clippy::too_many_arguments)] // page constructor wiring; grouping deferred
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        on_start: Rc<dyn Fn(Workout)>,
        on_view_fitness: Rc<dyn Fn()>,
        on_open_calendar: Rc<dyn Fn()>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        brief_store: Rc<BriefStore>,
        on_go_to_coaching: Rc<dyn Fn()>,
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

        // ── Header: greeting and date share one baseline ─────────────────────
        let header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let greeting_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .valign(gtk::Align::Baseline)
            .css_classes(["title-1"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Baseline)
            .css_classes(["dim-label"])
            .build();

        header_row.append(&greeting_label);
        header_row.append(&subtitle_label);
        inner.append(&header_row);

        // ── Today hero (rebuilt on each reload) ──────────────────────────────
        // The page's job is "what do I ride today?" — so today's workout,
        // drawn in its zone colours, comes before everything else.
        let dynamic_top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        inner.append(&dynamic_top);

        // ── Missing-key banner ───────────────────────────────────────────────
        // The dashboard is the page whose primary card is written by the AI,
        // and it was the one page without this — a rider with no key saw a
        // permanently empty Coach card and nothing explaining why.
        let api_key_banner = ApiKeyBanner::new("Add an API key to get your morning brief");
        inner.append(api_key_banner.widget());

        // ── Coach briefing card (static — not rebuilt on each reload) ────────
        let reload_holder: ReloadHolder = Rc::new(RefCell::new(None));
        let briefing_card = Self::build_briefing_card(
            pool.clone(),
            rt_handle.clone(),
            Rc::clone(&reload_holder),
            Rc::clone(&on_toast),
            Rc::clone(&brief_store),
            Rc::clone(&on_go_to_coaching),
        );
        inner.append(&briefing_card);

        // ── Form + recent activity (rebuilt on each reload) ──────────────────
        let dynamic = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        inner.append(&dynamic);

        clamp.set_child(Some(&inner));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        let reload: Rc<dyn Fn()> = {
            let greeting_label = greeting_label.clone();
            let subtitle_label = subtitle_label.clone();
            let dynamic_top = dynamic_top.clone();
            let dynamic = dynamic.clone();
            let briefing_card = briefing_card.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let athlete = Rc::clone(&athlete);
            let on_start = Rc::clone(&on_start);
            let on_view_fitness = Rc::clone(&on_view_fitness);
            let on_open_calendar = Rc::clone(&on_open_calendar);
            let reload_holder = Rc::clone(&reload_holder);
            let on_toast_reload = Rc::clone(&on_toast);

            Rc::new(move || {
                let now = Local::now();
                // Read the profile fresh on every reload — reload runs on each
                // navigation to the dashboard, so an FTP or name edit in
                // Preferences shows up as soon as the rider comes back here.
                let (ftp, name) = {
                    let a = athlete.borrow();
                    (a.ftp_watts, a.name.clone())
                };

                let salutation = match now.hour() {
                    5..=11 => "Good morning",
                    12..=17 => "Good afternoon",
                    _ => "Good evening",
                };
                greeting_label.set_label(&format!("{}, {}", salutation, name));

                let today_naive = now.date_naive();
                let weekday = today_naive.format("%A, %-d %B %Y").to_string();
                subtitle_label.set_label(&weekday);

                let today_str = today_naive.format("%Y-%m-%d").to_string();

                // Load everything the summary cards need off the main thread
                // (CLAUDE.md §2.3), then rebuild the dynamic areas on arrival.
                let pool_load = pool.clone();
                let dynamic_top = dynamic_top.clone();
                let dynamic = dynamic.clone();
                let briefing_card = briefing_card.clone();
                // Snapshot for the deferred UI build: taken now, at reload time,
                // so it is current. An `Rc` cannot cross `spawn_to_main`.
                let athlete_cb = athlete.borrow().clone();
                // The snapshot above is for drawing. The onboarding wizard also
                // has to *write* the shared cell afterwards, so keep the Rc as
                // well — `on_done` runs on the main thread and may hold it.
                let athlete_shared = Rc::clone(&athlete);
                let on_start = Rc::clone(&on_start);
                let on_view_fitness = Rc::clone(&on_view_fitness);
                let on_open_calendar = Rc::clone(&on_open_calendar);
                let reload_holder = Rc::clone(&reload_holder);
                let today_for_card = today_str.clone();
                let pool_ob = pool.clone();
                let rt_ob = rt_handle.clone();
                let on_toast_r = Rc::clone(&on_toast_reload);

                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { load_dashboard_data(&pool_load, &today_str).await },
                    move |result| {
                        // A failed load must not redraw the dashboard as empty:
                        // with no rides and no load it would show the welcome
                        // wizard, telling a rider with years of history that they
                        // are new here.
                        let DashboardData {
                            today_entry,
                            records,
                            suggested_workout,
                            ai_workout_detail,
                            ai_fitness_insight,
                            intervals_pairs,
                            first_use_done,
                        } = match result {
                            Ok(data) => data,
                            Err(e) => {
                                tracing::error!("Could not load dashboard data: {e}");
                                on_toast_r(
                                    adw::Toast::builder()
                                        .title("Could not load your dashboard")
                                        .timeout(5)
                                        .build(),
                                );
                                return;
                            }
                        };

                        // Compute TSB for readiness + 7-day trend
                        let now =
                            compute_load_metrics(&records, &intervals_pairs, ftp, today_naive);
                        let (ctl, atl) = (now.ctl, now.atl);
                        let tsb = now.tsb();
                        let week_ago = today_naive - Duration::days(7);
                        let then = compute_load_metrics(&records, &intervals_pairs, ftp, week_ago);
                        let (ctl_7d, atl_7d) = (then.ctl, then.atl);

                        while let Some(child) = dynamic_top.first_child() {
                            dynamic_top.remove(&child);
                        }
                        while let Some(child) = dynamic.first_child() {
                            dynamic.remove(&child);
                        }

                        // ── First-run onboarding ─────────────────────────────
                        if !first_use_done
                            && records.is_empty()
                            && today_entry.is_none()
                            && ctl == 0.0
                        {
                            briefing_card.set_visible(false);
                            let get_started_btn = gtk::Button::builder()
                                .label("Get Started")
                                .css_classes(["pill", "suggested-action"])
                                .tooltip_text("Run the first-use setup wizard")
                                .build();

                            let status = adw::StatusPage::builder()
                                .icon_name("media-playback-start-symbolic")
                                .title("Welcome to Cycle")
                                .description(
                                    "Set up your profile and connect your services to get \
                                     personalised training recommendations.",
                                )
                                .child(&get_started_btn)
                                .build();

                            let rh_ob = Rc::clone(&reload_holder);
                            let athlete_ob = Rc::clone(&athlete_shared);
                            get_started_btn.connect_clicked(move |btn| {
                                let root = btn.root().and_downcast::<gtk::Window>();
                                let pool_w = pool_ob.clone();
                                let rt_w = rt_ob.clone();
                                let rh_w = Rc::clone(&rh_ob);
                                let athlete_w = Rc::clone(&athlete_ob);
                                let pool_re = pool_ob.clone();
                                let rt_re = rt_ob.clone();
                                super::onboarding::show(
                                    root.as_ref(),
                                    pool_w,
                                    rt_w,
                                    Rc::new(move || {
                                        // The wizard writes straight to the database,
                                        // but every page, the workout engine and the
                                        // Preferences window read the shared athlete
                                        // cell, which still holds the profile loaded at
                                        // startup. Leaving it stale is why Preferences
                                        // opened on the old values and looked like
                                        // nothing had been saved. Re-read the row, put
                                        // it in the cell, then redraw.
                                        let athlete_c = Rc::clone(&athlete_w);
                                        let rh_c = Rc::clone(&rh_w);
                                        let pool_c = pool_re.clone();
                                        crate::ui::spawn_to_main(
                                            &rt_re,
                                            async move {
                                                db::load_or_create_athlete(&pool_c).await
                                            },
                                            move |result| {
                                                match result {
                                                    Ok(profile) => {
                                                        *athlete_c.borrow_mut() = profile;
                                                    }
                                                    Err(e) => tracing::error!(
                                                        "reloading profile after setup: {e}"
                                                    ),
                                                }
                                                // Clone the callback out before calling
                                                // it: reload() rebuilds these widgets and
                                                // holding the borrow across that risks a
                                                // re-entrant borrow (CLAUDE.md §2.4).
                                                let reload = rh_c.borrow().clone();
                                                if let Some(reload) = reload {
                                                    reload();
                                                }
                                            },
                                        );
                                    }),
                                );
                            });

                            dynamic_top.append(&status);
                            return;
                        }
                        briefing_card.set_visible(true);

                        // ── Today hero: the workout you're about to ride ──────
                        dynamic_top.append(&Self::build_workout_card(
                            today_entry,
                            suggested_workout,
                            &ai_workout_detail,
                            Rc::clone(&on_start),
                            &athlete_cb,
                            Rc::clone(&on_open_calendar),
                            pool_ob.clone(),
                            rt_ob.clone(),
                            Rc::clone(&reload_holder),
                            today_for_card.clone(),
                        ));

                        // ── Form + last activity list ─────────────────────────
                        dynamic.append(&Self::build_status_list(
                            ctl,
                            atl,
                            tsb,
                            ctl_7d,
                            atl_7d,
                            &ai_fitness_insight,
                            records.first(),
                            ftp,
                            Rc::clone(&on_view_fitness),
                            Rc::clone(&on_open_calendar),
                        ));
                    },
                );
            })
        };

        *reload_holder.borrow_mut() = Some(Rc::clone(&reload));

        // The Today hero and the form row read the brief from the cache, so
        // they have to be redrawn when a new one lands. The Coach card above
        // them updates itself through its own observer.
        brief_store.observe({
            let reload = Rc::clone(&reload);
            let last: RefCell<Option<String>> = RefCell::new(None);
            move |state: &BriefState| {
                // Only when the brief itself changed. The store also notifies
                // for status-only transitions, and redrawing the whole page on
                // each of those would re-query the database for nothing.
                let stamp = state
                    .brief
                    .as_ref()
                    .map(|b| format!("{}|{}", b.written_for, b.fingerprint));
                if last.borrow().as_ref() == stamp.as_ref() {
                    return;
                }
                *last.borrow_mut() = stamp;
                reload();
            }
        });

        reload();

        (Self { root }, reload)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    #[allow(clippy::too_many_arguments)]
    fn build_briefing_card(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload_holder: ReloadHolder,
        on_toast: Rc<dyn Fn(adw::Toast)>,
        brief_store: Rc<BriefStore>,
        on_go_to_coaching: Rc<dyn Fn()>,
    ) -> adw::PreferencesGroup {
        let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();

        let group = adw::PreferencesGroup::builder().title("Coach").build();

        // ── Briefing text label ───────────────────────────────────────────────
        let text_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["body"])
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .visible(false)
            .build();

        // ── Decision badge row ─────────────────────────────────────────────────
        let action_row = adw::ActionRow::builder().title("").visible(false).build();

        let decision_badge = gtk::Label::builder()
            .label("")
            .css_classes(["pill", "caption"])
            .valign(gtk::Align::Center)
            .build();
        action_row.add_suffix(&decision_badge);

        // Modify: "Replace" button (hidden unless decision is Modify)
        let use_workout_btn = gtk::Button::builder()
            .label("Replace")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Replace today's planned workout with the suggested workout")
            .build();
        action_row.add_suffix(&use_workout_btn);

        // Rest: "Remove from calendar" button (hidden unless decision is Rest)
        let remove_btn = gtk::Button::builder()
            .label("Remove from calendar")
            .css_classes(["pill", "destructive-action"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Remove today's workout — take a rest day as recommended")
            .build();
        action_row.add_suffix(&remove_btn);

        // Ease/Rest under a program: the plan owns the change, so send the
        // rider where it can be made properly rather than editing the calendar
        // behind the program's back.
        let see_plan_btn = gtk::Button::builder()
            .label("See plan")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("Open your program, where this session can be eased")
            .build();
        {
            let go = Rc::clone(&on_go_to_coaching);
            see_plan_btn.connect_clicked(move |_| go());
        }
        action_row.add_suffix(&see_plan_btn);

        // ── Text label as a fake row (inlined via Box) ────────────────────────
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        text_box.append(&text_label);

        // Wrap text_box in an ActionRow for consistent styling
        let content_row = adw::ActionRow::builder().visible(false).build();
        content_row.set_child(Some(&text_box));

        // ── Spinner + Refresh button ──────────────────────────────────────────
        // The subtitle is the provenance line: where what is shown came from,
        // and whether anything has happened since. It is the only place the
        // rider is told the brief has been overtaken, and pressing Refresh is
        // the only way a second request is ever made in a day.
        let generate_row = adw::ActionRow::builder()
            .title("Morning brief")
            .subtitle("")
            .build();

        let spinner = gtk::Spinner::builder().visible(false).build();
        generate_row.add_prefix(&spinner);

        // Plain pill — the Today hero keeps the page's single primary action.
        let generate_btn = gtk::Button::builder()
            .label("Refresh")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .tooltip_text("Ask your coach to write today's brief again")
            .build();
        {
            let store = Rc::clone(&brief_store);
            generate_btn.connect_clicked(move |_| store.refresh());
        }
        generate_row.add_suffix(&generate_btn);
        generate_row.set_activatable_widget(Some(&generate_btn));

        // Generate on top, then the decision, then the full briefing text.
        group.add(&generate_row);
        group.add(&action_row);
        group.add(&content_row);

        // Shared state: name of the AI-suggested alternative workout (Modify decision)
        let pending_alt_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Wire up "Replace" (Modify) — swaps today's calendar entry for the
        // suggested alternative; the Today hero then offers Start on it.
        {
            let pool_u = pool.clone();
            let rt_u = rt_handle.clone();
            let rh = Rc::clone(&reload_holder);
            let alt_name = Rc::clone(&pending_alt_name);
            let today = today_str.clone();
            let on_toast_alt = Rc::clone(&on_toast);
            use_workout_btn.connect_clicked(move |btn| {
                let name = match alt_name.borrow().clone() {
                    Some(n) if !n.is_empty() => n,
                    _ => return,
                };
                let pool_u = pool_u.clone();
                let rh = Rc::clone(&rh);
                let today = today.clone();
                let btn = btn.clone();
                let on_toast_swap = Rc::clone(&on_toast_alt);
                crate::ui::spawn_to_main(
                    &rt_u,
                    async move { swap_todays_workout(&pool_u, &name, &today).await },
                    move |result| match result {
                        Ok(()) => {
                            btn.set_visible(false);
                            if let Some(reload) = rh.borrow().as_ref() {
                                reload();
                            }
                        }
                        // The rider pressed a button and something has to happen.
                        // Previously this path only logged, so a missing workout
                        // or a failed write left the button sitting there.
                        Err(e) => {
                            tracing::error!("Could not swap in the alternative workout: {e}");
                            on_toast_swap(
                                adw::Toast::builder()
                                    .title("Could not switch to that workout")
                                    .timeout(5)
                                    .build(),
                            );
                        }
                    },
                );
            });
        }

        // Wire up "Remove from calendar" (Rest)
        {
            let pool_r = pool.clone();
            let rt_r = rt_handle.clone();
            let rh = Rc::clone(&reload_holder);
            let today = today_str.clone();
            remove_btn.connect_clicked(move |btn| {
                let pool = pool_r.clone();
                let rt = rt_r.clone();
                let rh = Rc::clone(&rh);
                let today = today.clone();
                crate::ui::widgets::dialog::confirm_destructive(
                    btn,
                    "Remove Today's Workout?",
                    "This will remove today's scheduled workout so you can rest as recommended.",
                    "_Remove",
                    move || {
                        let pool = pool.clone();
                        let rh = Rc::clone(&rh);
                        let today = today.clone();
                        crate::ui::spawn_to_main(
                            &rt,
                            async move {
                                // Load today's workout id first, then remove it.
                                if let Ok(Some(entry)) = db::load_today_entry(&pool, &today).await {
                                    if let Err(e) = db::delete_today_calendar_entry(
                                        &pool,
                                        entry.workout.id,
                                        &today,
                                    )
                                    .await
                                    {
                                        tracing::error!("delete_today_calendar_entry failed: {e}");
                                    }
                                }
                            },
                            move |()| {
                                if let Some(reload) = rh.borrow().as_ref() {
                                    reload();
                                }
                            },
                        );
                    },
                );
            });
        }

        // ── Subscribe to the daily brief ──────────────────────────────────────
        // The card never asks for anything itself. One request stands behind
        // this card, the Fitness page's and the Coaching page's, so whatever
        // they show cannot disagree — see ai::brief.
        brief_store.observe({
            let text_label = text_label.clone();
            let action_row = action_row.clone();
            let content_row = content_row.clone();
            let decision_badge = decision_badge.clone();
            let use_workout_btn = use_workout_btn.clone();
            let remove_btn = remove_btn.clone();
            let see_plan_btn = see_plan_btn.clone();
            let generate_row = generate_row.clone();
            let generate_btn = generate_btn.clone();
            let spinner = spinner.clone();
            let pending_alt = Rc::clone(&pending_alt_name);

            move |state: &BriefState| {
                generate_row.set_subtitle(state.provenance());
                generate_btn.set_sensitive(state.can_refresh());

                let loading = state.status == BriefStatus::Loading;
                spinner.set_visible(loading);
                if loading {
                    spinner.start();
                } else {
                    spinner.stop();
                }

                *pending_alt.borrow_mut() = state
                    .brief
                    .as_ref()
                    .and_then(|b| b.recommended_workout.clone());

                Self::apply_brief(
                    state,
                    &text_label,
                    &action_row,
                    &content_row,
                    &decision_badge,
                    &use_workout_btn,
                    &remove_btn,
                    &see_plan_btn,
                );
            }
        });

        group
    }

    /// Render one state of the brief onto the card.
    ///
    /// Which actions are offered turns on `program_active`, not on the verdict
    /// alone. Replacing or deleting a session a program scheduled would drop
    /// work the plan was counting on, and the plan never makes it up — so with
    /// a program running the card sends the rider to the Coaching page, where
    /// easing goes through the plan's own rules and is recorded.
    #[allow(clippy::too_many_arguments)]
    fn apply_brief(
        state: &BriefState,
        text_label: &gtk::Label,
        action_row: &adw::ActionRow,
        content_row: &adw::ActionRow,
        decision_badge: &gtk::Label,
        use_workout_btn: &gtk::Button,
        remove_btn: &gtk::Button,
        see_plan_btn: &gtk::Button,
    ) {
        // A failure gets its own sentence rather than a blank card. Previously
        // this path only logged, and a rider with no key saw nothing at all.
        if let BriefStatus::Failed(failure) = state.status {
            if state.brief.is_none() {
                text_label.set_text(failure.message());
                text_label.set_visible(true);
                content_row.set_visible(true);
                action_row.set_visible(false);
                return;
            }
        }
        if state.status == BriefStatus::NoApiKey && state.brief.is_none() {
            text_label.set_text(AiFailure::NoApiKey.message());
            text_label.set_visible(true);
            content_row.set_visible(true);
            action_row.set_visible(false);
            return;
        }

        let Some(brief) = &state.brief else {
            content_row.set_visible(false);
            action_row.set_visible(false);
            return;
        };

        let prose = brief.full_prose();
        if prose.trim().is_empty() {
            content_row.set_visible(false);
        } else {
            text_label.set_markup(&to_pango(&prose));
            text_label.set_visible(true);
            content_row.set_visible(true);
        }
        action_row.set_visible(true);

        let has_planned = brief.planned_workout.is_some();
        // Only ever `Some` when no program owns the day — enforced in
        // ai::brief::parse, so this is safe to act on.
        let alternative = brief.recommended_workout.as_deref();

        // With a program running, easing is the Coaching page's job.
        let defer_to_plan = brief.program_active && brief.verdict != CoachVerdict::Proceed;
        see_plan_btn.set_visible(defer_to_plan);

        match brief.verdict {
            CoachVerdict::Proceed => {
                // "As planned" only makes sense when something is on the calendar.
                if has_planned {
                    action_row.set_title("Proceed as planned");
                    action_row.set_subtitle("Your readiness supports today's workout.");
                } else {
                    action_row.set_title("Ready to train");
                    action_row.set_subtitle("Your readiness supports training today.");
                }
                decision_badge.set_label("Proceed");
                decision_badge.set_css_classes(&["pill", "caption", "success"]);
                use_workout_btn.set_visible(false);
                remove_btn.set_visible(false);
            }
            CoachVerdict::Ease => {
                let title = match alternative {
                    Some(name) => format!("Ride easier → {name}"),
                    None => "Ride easier today".to_string(),
                };
                action_row.set_title(&title);
                action_row.set_subtitle(if defer_to_plan {
                    "Your program can ease this session for you."
                } else {
                    "Your readiness suggests a lighter session today."
                });
                decision_badge.set_label("Ease");
                decision_badge.set_css_classes(&["pill", "caption", "warning"]);
                use_workout_btn.set_visible(alternative.is_some());
                remove_btn.set_visible(false);
            }
            CoachVerdict::Rest => {
                action_row.set_title("Rest today");
                action_row.set_subtitle(if defer_to_plan {
                    "Your program can ease this session for you."
                } else {
                    "Recovery takes priority over training today."
                });
                decision_badge.set_label("Rest");
                decision_badge.set_css_classes(&["pill", "caption", "error"]);
                use_workout_btn.set_visible(false);
                // Nothing to remove when nothing is scheduled — and never a
                // programmed session, which the plan is still counting on.
                remove_btn.set_visible(has_planned && !brief.program_active);
            }
        }
    }

    /// The Today hero — exactly one of three states:
    /// a scheduled workout (Start), the AI suggestion standing in for an
    /// empty day (Schedule / Start), or a rest day linking to the Calendar.
    #[allow(clippy::too_many_arguments)]
    fn build_workout_card(
        entry: Option<db::TodayEntry>,
        suggested: Option<Workout>,
        ai_detail: &str,
        on_start: Rc<dyn Fn(Workout)>,
        athlete: &AthleteProfile,
        on_open_calendar: Rc<dyn Fn()>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        reload_holder: ReloadHolder,
        today_str: String,
    ) -> gtk::Box {
        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();

        outer.append(
            &gtk::Label::builder()
                .label("Today")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        match (entry, suggested) {
            // ── Scheduled workout ────────────────────────────────────────────
            (Some(entry), _) => {
                let subtitle = if entry.workout.description.trim().is_empty() {
                    Self::workout_meta(&entry.workout)
                } else {
                    format!(
                        "{} — {}",
                        Self::workout_meta(&entry.workout),
                        entry.workout.description.trim()
                    )
                };
                let (card, action_area) = Self::hero_card(&entry.workout, athlete, &subtitle);

                if entry.completed {
                    let done = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(6)
                        .valign(gtk::Align::Center)
                        .build();
                    done.append(
                        &gtk::Image::builder()
                            .icon_name("object-select-symbolic")
                            .css_classes(["success"])
                            .build(),
                    );
                    done.append(
                        &gtk::Label::builder()
                            .label("Completed")
                            .css_classes(["success", "caption-heading"])
                            .build(),
                    );
                    action_area.append(&done);
                } else {
                    let start_btn = gtk::Button::builder()
                        .label("Start")
                        .css_classes(["suggested-action", "pill"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Start today's scheduled workout")
                        .build();
                    let w = entry.workout.clone();
                    start_btn.connect_clicked(move |_| on_start(w.clone()));
                    action_area.append(&start_btn);
                }

                outer.append(&card);
            }
            // ── Nothing scheduled — the AI suggestion becomes the hero ───────
            (None, Some(workout)) => {
                let subtitle = if ai_detail.is_empty() {
                    Self::workout_meta(&workout)
                } else {
                    format!("{} — {}", Self::workout_meta(&workout), ai_detail)
                };
                let (card, action_area) = Self::hero_card(&workout, athlete, &subtitle);

                // Schedule: put the suggestion on today's calendar, then reload
                // so this card becomes the scheduled hero.
                let schedule_btn = gtk::Button::builder()
                    .label("Schedule")
                    .css_classes(["pill"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Add this workout to today's calendar")
                    .build();
                {
                    let pool = pool.clone();
                    let workout_id = workout.id;
                    schedule_btn.connect_clicked(move |_| {
                        let pool = pool.clone();
                        let today = today_str.clone();
                        let rh = Rc::clone(&reload_holder);
                        crate::ui::spawn_to_main(
                            &rt_handle,
                            async move {
                                if let Err(e) =
                                    db::schedule_workout(&pool, workout_id, &today, None).await
                                {
                                    tracing::error!("schedule suggested workout: {e}");
                                }
                            },
                            move |()| {
                                if let Some(reload) = rh.borrow().as_ref() {
                                    reload();
                                }
                            },
                        );
                    });
                }
                action_area.append(&schedule_btn);

                // Start: the day's primary action when nothing else is planned.
                let start_btn = gtk::Button::builder()
                    .label("Start")
                    .css_classes(["suggested-action", "pill"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Start the suggested workout now")
                    .build();
                start_btn.connect_clicked(move |_| on_start(workout.clone()));
                action_area.append(&start_btn);

                outer.append(&card);
            }
            // ── Rest day ─────────────────────────────────────────────────────
            (None, None) => {
                let list = gtk::ListBox::builder()
                    .css_classes(["boxed-list"])
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                // The row performs the action its copy names: click → Calendar.
                let row = adw::ActionRow::builder()
                    .title("Rest Day")
                    .subtitle("No workout scheduled — plan one in Calendar")
                    .activatable(true)
                    .tooltip_text("Open Calendar to plan a workout")
                    .build();
                row.add_prefix(
                    &gtk::Image::builder()
                        .icon_name("weather-clear-night-symbolic")
                        .css_classes(["dim-label"])
                        .build(),
                );
                row.add_suffix(
                    &gtk::Image::builder()
                        .icon_name("go-next-symbolic")
                        .css_classes(["dim-label"])
                        .build(),
                );
                row.connect_activated(move |_| on_open_calendar());
                list.append(&row);
                outer.append(&list);
            }
        }

        outer
    }

    /// Meta line shared by hero cards: duration · TSS · category.
    fn workout_meta(w: &Workout) -> String {
        format!(
            "{} min · TSS {} · {}",
            w.duration_secs / 60,
            w.tss as u32,
            w.category.label()
        )
    }

    /// Card with the workout's zone-coloured profile, name, and subtitle.
    /// Returns `(card, action_area)` — the caller appends its own buttons.
    fn hero_card(
        workout: &Workout,
        athlete: &AthleteProfile,
        subtitle: &str,
    ) -> (gtk::Box, gtk::Box) {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .build();

        let graph = WorkoutGraph::new(workout, athlete.ftp_watts);
        graph.widget().set_content_height(72);
        graph.widget().set_margin_top(12);
        graph.widget().set_margin_start(12);
        graph.widget().set_margin_end(12);
        card.append(graph.widget());

        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let text_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .build();
        text_col.append(
            &gtk::Label::builder()
                .label(&workout.name)
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["title-2"])
                .build(),
        );
        text_col.append(
            &gtk::Label::builder()
                .label(subtitle)
                .halign(gtk::Align::Start)
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        row.append(&text_col);
        card.append(&row);

        (card, row)
    }

    /// One quiet list holding form (readiness + CTL/ATL/TSB) and the most
    /// recent activity — replaces the old banner + two-card row.
    #[allow(clippy::too_many_arguments)]
    fn build_status_list(
        ctl: f64,
        atl: f64,
        tsb: f64,
        ctl_7d: f64,
        atl_7d: f64,
        insight: &str,
        record: Option<&db::SessionSummary>,
        ftp: u32,
        on_view_fitness: Rc<dyn Fn()>,
        on_open_calendar: Rc<dyn Fn()>,
    ) -> gtk::ListBox {
        let list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();

        // ── Form row: readiness in plain words, numbers as quiet pills ───────
        // The same sentence the Fitness page leads with — this row is the same
        // statement about the same number, so it must not word it differently.
        let message = crate::training::fitness::tsb_status_text(tsb);

        let form_row = adw::ActionRow::builder()
            .title(message)
            .activatable(true)
            .tooltip_text("Open Fitness for the full picture")
            // AI text may contain markup-significant characters (& etc.)
            .use_markup(false)
            .build();
        let preview = insight_preview(insight);
        if !preview.is_empty() {
            form_row.set_subtitle(&preview);
        }

        let trend_arrow = |current: f64, prev: f64| -> &'static str {
            if current > prev + 1.0 {
                "↑"
            } else if current < prev - 1.0 {
                "↓"
            } else {
                "→"
            }
        };

        // CTL and ATL only — the readiness message above already expresses
        // their balance (TSB), so a third number would say nothing new.
        for (label, value, trend, tip) in [
            (
                "CTL",
                format!("{:.0}", ctl),
                trend_arrow(ctl, ctl_7d),
                "Chronic Training Load — 42-day fitness average. Higher means more aerobic base built up.",
            ),
            (
                "ATL",
                format!("{:.0}", atl),
                trend_arrow(atl, atl_7d),
                "Acute Training Load — 7-day fatigue average. Spikes after hard weeks.",
            ),
        ] {
            let pair = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .tooltip_text(tip)
                .valign(gtk::Align::Center)
                .build();
            pair.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            pair.append(
                &gtk::Label::builder()
                    .label(format!("{value}{trend}"))
                    .css_classes(["caption-heading", "numeric"])
                    .build(),
            );
            form_row.add_suffix(&pair);
        }

        form_row.add_suffix(
            &gtk::Image::builder()
                .icon_name("go-next-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        form_row.connect_activated(move |_| on_view_fitness());
        list.append(&form_row);

        // ── Last activity row ────────────────────────────────────────────────
        let last_row = match record {
            None => adw::ActionRow::builder()
                .title("No sessions recorded yet")
                .subtitle("Complete a workout to see it here")
                .build(),
            // Workout names are user/AI-supplied — markup disabled below.
            Some(r) => {
                let local_dt = r.started_at.with_timezone(&Local);
                let title = r.workout_name.as_deref().unwrap_or("Free Ride");
                let mins = r.duration_secs as u32 / 60;
                let power_str = match r.normalised_power {
                    Some(np) => format!("{} W NP", np as u32),
                    None => match r.average_power {
                        Some(avg) => format!("{} W avg", avg as u32),
                        None => String::new(),
                    },
                };
                let tss_str = r
                    .tss(ftp)
                    .map(|t| format!(" · TSS {:.0}", t))
                    .unwrap_or_default();
                let detail = if power_str.is_empty() {
                    format!("{} min{}", mins, tss_str)
                } else {
                    format!("{} min · {}{}", mins, power_str, tss_str)
                };
                adw::ActionRow::builder()
                    .title(title)
                    .use_markup(false)
                    .subtitle(format!("{} — {}", local_dt.format("%-d %b"), detail))
                    .build()
            }
        };
        last_row.add_prefix(
            &gtk::Image::builder()
                .icon_name("document-open-recent-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        // Past sessions live in the Calendar — the row takes you to them.
        if record.is_some() {
            last_row.set_activatable(true);
            last_row.set_tooltip_text(Some("Open Calendar to review past sessions"));
            last_row.add_suffix(
                &gtk::Image::builder()
                    .icon_name("go-next-symbolic")
                    .css_classes(["dim-label"])
                    .build(),
            );
            last_row.connect_activated(move |_| on_open_calendar());
        }
        list.append(&last_row);

        list
    }
}
