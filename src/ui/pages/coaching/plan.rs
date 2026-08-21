//! "Your Program" — the plan the rider is living with, and what should change
//! about it.
//!
//! The card has three faces, and shows exactly one:
//!
//! * a program is being followed → where it has got to, what was missed, and
//!   any adjustment the rules propose;
//! * no program, but the calendar holds scheduled workouts that belong to none
//!   → an offer to adopt them, so a rider who plainly has a plan is not told
//!   they have none;
//! * neither → nothing at all. The Build Program section below is the answer.
//!
//! The adjustments come from [`crate::training::program`], which is pure and
//! costs nothing to run. Only "Rebuild with AI" spends the rider's key.

use adw::prelude::*;
use chrono::{Datelike, Duration as CDuration, Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, db, workout::Workout};
use crate::training::fitness::TsbBand;
use crate::training::program::{plan_view, Adjustment, CoachVerdict, Phase, ProgramStatus};

use super::data::{load_plan_data, PlanData};
use super::program::week_start;

pub struct PlanCard {
    root: gtk::Box,
    group: adw::PreferencesGroup,
    /// Rows added per reload, tracked so they can be removed cleanly —
    /// AdwPreferencesGroup's first_child() returns internal layout widgets.
    rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    apply_btn: gtk::Button,
    rebuild_btn: gtk::Button,
    end_btn: gtk::Button,
    adopt_btn: gtk::Button,
    actions: gtk::Box,
    /// What "Apply Adjustments" will write, as of the last reload.
    pending: Rc<RefCell<Vec<Adjustment>>>,
    /// The program on screen, so the action buttons know what they act on.
    program_id: Rc<RefCell<Option<i64>>>,
    /// The state the card was last drawn from, which the AI rebuild describes
    /// to the coach rather than reading the whole plan a second time.
    last_state: Rc<RefCell<Option<ProgramStatus>>>,
    /// What the morning brief made of today, as of the last time it changed.
    ///
    /// Held rather than passed in because the card reloads on navigation and
    /// the brief arrives on its own schedule; whichever happens last must still
    /// see the other.
    verdict: Rc<std::cell::Cell<CoachVerdict>>,
    athlete: Rc<RefCell<AthleteProfile>>,
    workouts: Rc<Vec<Workout>>,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_toast: Rc<dyn Fn(adw::Toast)>,
}

impl PlanCard {
    pub fn new(
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
        workouts: Rc<Vec<Workout>>,
        on_toast: Rc<dyn Fn(adw::Toast)>,
    ) -> Rc<Self> {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();

        let group = adw::PreferencesGroup::builder()
            .title("Your Program")
            .build();
        root.append(&group);

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Start)
            .build();

        let apply_btn = gtk::Button::builder()
            .label("Apply Adjustments")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Change the upcoming session as suggested")
            .sensitive(false)
            .build();
        let rebuild_btn = gtk::Button::builder()
            .label("Rebuild with AI")
            .css_classes(["pill"])
            .tooltip_text(
                "Ask the AI Coach to replan the remaining weeks around what you have \
                 actually ridden. This sends one request to your AI provider.",
            )
            .build();
        let adopt_btn = gtk::Button::builder()
            .label("Track These as a Program")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Follow the workouts already on your calendar as one program")
            .visible(false)
            .build();
        let end_btn = gtk::Button::builder()
            .label("End Program")
            .css_classes(["pill", "destructive-action"])
            .tooltip_text("Stop following this program. Your calendar is left as it is.")
            .hexpand(true)
            .halign(gtk::Align::End)
            .build();

        actions.append(&apply_btn);
        actions.append(&rebuild_btn);
        actions.append(&adopt_btn);
        actions.append(&end_btn);
        root.append(&actions);

        let card = Rc::new(Self {
            root,
            group,
            rows: Rc::new(RefCell::new(Vec::new())),
            apply_btn,
            rebuild_btn,
            end_btn,
            adopt_btn,
            actions,
            pending: Rc::new(RefCell::new(Vec::new())),
            program_id: Rc::new(RefCell::new(None)),
            last_state: Rc::new(RefCell::new(None)),
            verdict: Rc::new(std::cell::Cell::new(CoachVerdict::Proceed)),
            athlete,
            workouts,
            pool,
            rt_handle,
            on_toast,
        });

        card.connect_apply();
        card.connect_end();
        card.connect_adopt();
        card.connect_rebuild();
        card
    }

    /// Put an eased session back to what the program originally asked for.
    fn undo(self: &Rc<Self>, entry_id: i64) {
        let pool = self.pool.clone();
        let card = Rc::clone(self);
        let on_toast = Rc::clone(&self.on_toast);

        crate::ui::spawn_to_main(
            &self.rt_handle.clone(),
            async move { db::revert_adjustment(&pool, entry_id).await },
            move |result| {
                match result {
                    Ok(true) => card.reload(),
                    // The entry was ridden or removed since the row was drawn.
                    // Reloading is still right — it makes the stale row go away.
                    Ok(false) => {
                        tracing::warn!("adjustment {entry_id} was no longer revertible");
                        on_toast(
                            adw::Toast::builder()
                                .title("That session has already been ridden")
                                .timeout(5)
                                .build(),
                        );
                        card.reload();
                    }
                    Err(e) => {
                        tracing::error!("reverting adjustment {entry_id}: {e}");
                        on_toast(
                            adw::Toast::builder()
                                .title("Could not put that session back")
                                .timeout(5)
                                .build(),
                        );
                    }
                };
            },
        );
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Read the program's state and redraw. Safe to call on every page visit.
    /// Record what the morning brief said, and redraw if it changed.
    ///
    /// Only a change redraws: the store notifies on every state transition,
    /// including ones that say nothing about the verdict, and reloading the
    /// plan on each of those would re-query the database for nothing.
    pub fn set_verdict(self: &Rc<Self>, verdict: CoachVerdict) {
        if self.verdict.replace(verdict) != verdict {
            self.reload();
        }
    }

    pub fn reload(self: &Rc<Self>) {
        let today = Local::now().date_naive();
        let ftp = self.athlete.borrow().ftp_watts;
        let card = Rc::clone(self);
        let pool = self.pool.clone();

        crate::ui::spawn_to_main(
            &self.rt_handle.clone(),
            async move { load_plan_data(&pool, today, ftp).await },
            move |result| match result {
                Ok(data) => card.render(data, today),
                // The card describes itself or it is not shown. An empty one
                // would read as "you have no program", which is a claim about
                // the rider rather than about the database.
                Err(e) => {
                    tracing::error!("Could not load your program: {e}");
                    card.root.set_visible(false);
                }
            },
        );
    }

    fn clear_rows(&self) {
        for row in self.rows.borrow().iter() {
            self.group.remove(row);
        }
        self.rows.borrow_mut().clear();
    }

    fn add_row(&self, row: adw::ActionRow) {
        self.group.add(&row);
        self.rows.borrow_mut().push(row);
    }

    fn render(self: &Rc<Self>, data: PlanData, today: NaiveDate) {
        self.clear_rows();
        self.pending.borrow_mut().clear();
        *self.last_state.borrow_mut() = None;
        *self.program_id.borrow_mut() = data.program.as_ref().map(|p| p.id);

        let Some(program) = data.program else {
            self.render_orphans(data.orphans);
            return;
        };

        self.adopt_btn.set_visible(false);
        self.rebuild_btn.set_visible(true);
        self.end_btn.set_visible(true);

        // The brief says how hard today should be; these rules decide what
        // that means for the plan, and are the only thing that produces an
        // adjustment. One authority, so the rider gets one answer — the
        // calendar reads the same `plan_view`.
        let (state, adjustments) = plan_view(
            &program,
            &data.sessions,
            &data.metrics,
            &data.wellness,
            &self.workouts,
            today,
            self.verdict.get(),
        );

        self.group.set_description(Some(&Self::describe(&state)));
        self.render_progress(&state, data.metrics.tsb(), today);
        self.render_adjustments(&adjustments, &state);

        *self.pending.borrow_mut() = adjustments;
        self.apply_btn
            .set_sensitive(!self.pending.borrow().is_empty());
        *self.last_state.borrow_mut() = Some(state);
        self.root.set_visible(true);
    }

    /// "Week 3 of 12 · Build"
    fn describe(state: &ProgramStatus) -> String {
        format!(
            "Week {} of {} · {}",
            state.week,
            state.total_weeks,
            state.phase.label()
        )
    }

    fn render_progress(&self, state: &ProgramStatus, tsb: f64, today: NaiveDate) {
        let row = adw::ActionRow::builder()
            .title(format!(
                "{} of {} sessions completed",
                state.completed, state.planned
            ))
            .subtitle(format!(
                "Form {tsb:+.0} — {}",
                TsbBand::of(tsb).status_text()
            ))
            .build();
        self.add_row(row);

        if !state.missed.is_empty() {
            let last = state.missed.last().expect("checked non-empty").date;
            let row = adw::ActionRow::builder()
                .title(format!(
                    "{} session{} missed",
                    state.missed.len(),
                    if state.missed.len() == 1 { "" } else { "s" }
                ))
                // Saying so plainly, because the plan will not try to claw them
                // back and the rider should know that is deliberate.
                .subtitle(format!(
                    "Most recently {} ({}). Missed sessions are not rescheduled — \
                     the plan carries on from here.",
                    last.format("%-d %B"),
                    days_ago(last, today)
                ))
                .subtitle_lines(3)
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("dialog-warning-symbolic")
                    .css_classes(["warning"])
                    .build(),
            );
            self.add_row(row);
        }
    }

    fn render_adjustments(self: &Rc<Self>, adjustments: &[Adjustment], state: &ProgramStatus) {
        self.render_already_eased(state);

        if adjustments.is_empty() {
            let (title, subtitle) = Self::nothing_to_change(state);
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .subtitle_lines(3)
                .build();
            self.add_row(row);
            return;
        }

        for adj in adjustments {
            let row = adw::ActionRow::builder()
                .title(format!(
                    "{}  {} → {}",
                    adj.date.format("%a %-d %b"),
                    adj.from_name,
                    adj.to_name
                ))
                .subtitle(adj.reason.text())
                .subtitle_lines(3)
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("view-refresh-symbolic")
                    .css_classes(["accent"])
                    .build(),
            );
            self.add_row(row);
        }
    }

    /// Upcoming sessions that have already been eased, each with a way back.
    ///
    /// An adjustment the rider cannot reverse is a change made to them rather
    /// than for them, and they are the one who knows whether yesterday's
    /// reading was a bad night or the start of something.
    fn render_already_eased(self: &Rc<Self>, state: &ProgramStatus) {
        for session in state.upcoming.iter() {
            let Some(original) = session.adjusted_from.clone() else {
                continue;
            };

            let row = adw::ActionRow::builder()
                .title(format!(
                    "{}  {}",
                    session.date.format("%a %-d %b"),
                    session.workout_name
                ))
                .subtitle(format!("Eased from {original}"))
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("object-select-symbolic")
                    .css_classes(["success"])
                    .build(),
            );

            let undo = gtk::Button::builder()
                .label("Undo")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .tooltip_text(format!("Put {original} back on this day"))
                .build();

            // Weak, not strong: this row is rebuilt on every reload, and a
            // strong capture would put the card inside a widget the card owns
            // (CLAUDE.md §2.4).
            let card = Rc::downgrade(self);
            let entry_id = session.entry_id;
            undo.connect_clicked(move |_| {
                let Some(card) = card.upgrade() else { return };
                card.undo(entry_id);
            });
            row.add_suffix(&undo);

            self.add_row(row);
        }
    }

    /// What to say when the rules propose nothing — which is most weeks, and
    /// should read as a verdict rather than as an absence.
    fn nothing_to_change(state: &ProgramStatus) -> (&'static str, String) {
        if state.phase == Phase::Recovery {
            return (
                "Recovery week — nothing to change",
                "This week is already the easy one. Ride it as written.".to_string(),
            );
        }
        if state.upcoming.is_empty() {
            return (
                "No sessions left to adjust",
                "Every planned session has been ridden or has passed.".to_string(),
            );
        }
        if !state.missed.is_empty() {
            return (
                "The plan still fits",
                "You have missed sessions, but your form says you can take the next \
                 one as planned. Nothing is being added to catch up."
                    .to_string(),
            );
        }
        (
            "The plan still fits",
            "Your form, sleep and recent sessions all say to ride it as written.".to_string(),
        )
    }

    /// The face shown when the calendar holds a plan the app is not tracking.
    fn render_orphans(&self, orphans: Option<(NaiveDate, NaiveDate, i64)>) {
        let Some((first, last, count)) = orphans else {
            self.root.set_visible(false);
            return;
        };

        self.group.set_description(None);
        let row = adw::ActionRow::builder()
            .title(format!(
                "{count} scheduled workouts aren't part of a program"
            ))
            .subtitle(format!(
                "They run from {} to {}. Track them and this page can tell you \
                 what you have missed and what to change.",
                first.format("%-d %B"),
                last.format("%-d %B %Y")
            ))
            .subtitle_lines(3)
            .build();
        self.add_row(row);

        self.adopt_btn.set_visible(true);
        self.apply_btn.set_sensitive(false);
        self.rebuild_btn.set_visible(false);
        self.end_btn.set_visible(false);
        self.root.set_visible(true);
    }

    fn connect_apply(self: &Rc<Self>) {
        let pool = self.pool.clone();
        let rt_handle = self.rt_handle.clone();
        let on_toast = Rc::clone(&self.on_toast);
        let card = Rc::clone(self);
        self.apply_btn.connect_clicked(move |_| {
            let adjustments = card.pending.borrow().clone();
            if adjustments.is_empty() {
                return;
            }

            let pool_write = pool.clone();
            let card_after = Rc::clone(&card);
            let on_toast = Rc::clone(&on_toast);

            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    let mut applied = 0u32;
                    for adj in &adjustments {
                        match db::apply_adjustment(&pool_write, adj.entry_id, adj.to_workout_id)
                            .await
                        {
                            Ok(true) => applied += 1,
                            Ok(false) => tracing::warn!(
                                "adjustment to {} changed nothing — already ridden or gone",
                                adj.entry_id
                            ),
                            Err(e) => {
                                tracing::error!("applying adjustment to {}: {e}", adj.entry_id)
                            }
                        }
                    }
                    (applied, adjustments.len() as u32)
                },
                move |(applied, total)| {
                    let msg = if applied == total {
                        format!(
                            "{applied} session{} adjusted",
                            if applied == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("{applied} of {total} adjusted — see the log")
                    };
                    on_toast(adw::Toast::builder().title(msg).timeout(5).build());
                    card_after.reload();
                },
            );
        });
    }

    fn connect_end(self: &Rc<Self>) {
        let pool = self.pool.clone();
        let rt_handle = self.rt_handle.clone();
        let on_toast = Rc::clone(&self.on_toast);
        let card = Rc::clone(self);
        self.end_btn.connect_clicked(move |btn| {
            let Some(id) = *card.program_id.borrow() else {
                return;
            };

            let dialog = adw::AlertDialog::new(
                Some("End this program?"),
                Some(
                    "The workouts already on your calendar stay where they are. \
                     This page will stop tracking them.",
                ),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("end", "End Program");
            dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");

            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let on_toast = Rc::clone(&on_toast);
            let card = Rc::clone(&card);

            dialog.connect_response(None, move |_, response| {
                if response != "end" {
                    return;
                }
                let pool_write = pool.clone();
                let card = Rc::clone(&card);
                let on_toast = Rc::clone(&on_toast);

                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { db::deactivate_program(&pool_write, id).await },
                    move |result| {
                        match result {
                            Ok(()) => card.reload(),
                            Err(e) => {
                                tracing::error!("ending the program: {e}");
                                on_toast(
                                    adw::Toast::builder()
                                        .title("Could not end the program")
                                        .timeout(5)
                                        .build(),
                                );
                            }
                        };
                    },
                );
            });

            dialog.present(Some(btn));
        });
    }

    fn connect_adopt(self: &Rc<Self>) {
        let pool = self.pool.clone();
        let rt_handle = self.rt_handle.clone();
        let on_toast = Rc::clone(&self.on_toast);
        let card = Rc::clone(self);
        self.adopt_btn.connect_clicked(move |_| {
            let pool_read = pool.clone();
            let card = Rc::clone(&card);
            let on_toast = Rc::clone(&on_toast);

            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    let Some((first, last, _)) = db::orphan_entry_span(&pool_read).await? else {
                        return anyhow::Ok(0);
                    };
                    // The program starts on the Monday of the first scheduled
                    // week so its weeks line up with the calendar's, and runs
                    // long enough to cover the last session.
                    let start = week_start(first);
                    let weeks = ((last - start).num_days() / 7 + 1).max(1) as u32;
                    let id = db::save_program(&pool_read, start, weeks, "").await?;
                    let adopted = db::adopt_orphan_entries(&pool_read, id).await?;
                    anyhow::Ok(adopted)
                },
                move |result| {
                    match result {
                        Ok(adopted) => {
                            on_toast(
                                adw::Toast::builder()
                                    .title(format!("Now tracking {adopted} scheduled workouts"))
                                    .timeout(5)
                                    .build(),
                            );
                            card.reload();
                        }
                        Err(e) => {
                            tracing::error!("adopting scheduled workouts: {e}");
                            on_toast(
                                adw::Toast::builder()
                                    .title("Could not track your scheduled workouts")
                                    .timeout(5)
                                    .build(),
                            );
                        }
                    };
                },
            );
        });
    }

    /// Replan the remaining weeks with the AI coach.
    ///
    /// The only control on this card that spends the rider's key, so it is the
    /// only one behind an explicit press. The reply replaces the plan from next
    /// Monday: the current week is left alone, because a plan that changes
    /// under a rider mid-week is worse than one that waits until Monday.
    fn connect_rebuild(self: &Rc<Self>) {
        let pool = self.pool.clone();
        let rt_handle = self.rt_handle.clone();
        let on_toast = Rc::clone(&self.on_toast);
        let card = Rc::clone(self);
        self.rebuild_btn.connect_clicked(move |_| {
            let Some(program_id) = *card.program_id.borrow() else {
                return;
            };
            let api_key =
                match crate::data::keystore::get_secret(crate::data::keystore::KEY_ANTHROPIC) {
                    Ok(Some(k)) if !k.trim().is_empty() => k,
                    _ => {
                        on_toast(
                            adw::Toast::builder()
                                .title(
                                    "No AI provider key configured. Enter your API key in \
                                 Preferences → Integrations.",
                                )
                                .timeout(6)
                                .build(),
                        );
                        return;
                    }
                };

            // Everything held behind a non-Send Rc is read here, on the main
            // thread, before any of it crosses to the runtime.
            let today = Local::now().date_naive();
            let profile = card.athlete.borrow().clone();
            let library: Vec<Workout> = (*card.workouts).clone();
            let state = card.last_state.borrow().clone();
            let Some(state) = state else { return };

            card.set_busy(true);
            on_toast(
                adw::Toast::builder()
                    .title("Asking the coach to replan your remaining weeks…")
                    .timeout(4)
                    .build(),
            );

            let pool_task = pool.clone();
            let card_after = Rc::clone(&card);
            let on_toast = Rc::clone(&on_toast);

            crate::ui::spawn_to_main(
                &rt_handle,
                async move {
                    rebuild_program(
                        pool_task, api_key, program_id, state, profile, library, today,
                    )
                    .await
                },
                move |result| {
                    card_after.set_busy(false);
                    let msg = match result {
                        Ok(count) => format!("Remaining weeks replanned — {count} sessions"),
                        Err(e) => {
                            tracing::error!("rebuilding the program: {e}");
                            "Could not replan your program — nothing was changed".to_string()
                        }
                    };
                    on_toast(adw::Toast::builder().title(msg).timeout(6).build());
                    card_after.reload();
                },
            );
        });
    }

    /// Grey the actions out while a rebuild is in flight, so the plan cannot be
    /// adjusted from underneath a reply that is about to replace it.
    fn set_busy(&self, busy: bool) {
        self.actions.set_sensitive(!busy);
    }
}

/// The Monday after `date` — where a replanned program picks up.
fn next_monday(date: NaiveDate) -> NaiveDate {
    let ahead = 7 - date.weekday().num_days_from_monday() as i64;
    date + CDuration::days(ahead)
}

/// Ask the coach for revised weeks and write them to the calendar.
///
/// Runs entirely on the tokio runtime. Nothing is deleted until a usable reply
/// is in hand: a failed request, or one that parses to nothing, must leave the
/// rider with the plan they already had.
async fn rebuild_program(
    pool: SqlitePool,
    api_key: String,
    program_id: i64,
    state: ProgramStatus,
    profile: AthleteProfile,
    library: Vec<Workout>,
    today: NaiveDate,
) -> anyhow::Result<usize> {
    use crate::ai::coach::{
        build_program_revision_prompt, get_suggestion, parse_program_response,
        ProgramRevisionContext,
    };
    use crate::ai::context::{day_name_to_offset, wellness_snapshots, workouts_as_options};

    let data = super::data::load_program_prompt_data(&pool, today).await?;
    let program = db::active_program(&pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the program ended while it was being replanned"))?;

    let metrics = crate::training::fitness::compute_load_metrics(
        &data.records,
        &data.intervals_pairs,
        profile.ftp_watts,
        today,
    );

    // The training days the program was built around; a program adopted from a
    // bare calendar has none recorded, so the days it actually uses stand in.
    let training_days: Vec<String> = if program.training_days.trim().is_empty() {
        let mut days: Vec<String> = state
            .upcoming
            .iter()
            .chain(state.missed.iter())
            .map(|s| s.date.format("%A").to_string().to_lowercase())
            .collect();
        days.sort();
        days.dedup();
        if days.is_empty() {
            vec!["monday".into(), "wednesday".into(), "friday".into()]
        } else {
            days
        }
    } else {
        program
            .training_days
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect()
    };

    let recent_missed: Vec<String> = state
        .missed
        .iter()
        .rev()
        .take(6)
        .map(|s| format!("{} — {}", s.date.format("%a %-d %b"), s.workout_name))
        .collect();

    let ctx = ProgramRevisionContext {
        athlete: profile,
        ctl: metrics.ctl,
        tsb: metrics.tsb(),
        goals: data.goals,
        athlete_context: data.athlete_ctx,
        workout_options: workouts_as_options(&library, &data.icu_workouts),
        training_days,
        current_week: state.week,
        weeks_remaining: state.total_weeks.saturating_sub(state.week),
        completed: state.completed,
        missed: state.missed.len(),
        recent_missed,
        wellness: wellness_snapshots(&data.wellness),
        time_off: data
            .time_off
            .iter()
            .map(|t| t.date.format("%Y-%m-%d").to_string())
            .collect(),
    };

    let reply = get_suggestion(&api_key, &build_program_revision_prompt(&ctx), 2800).await?;
    let entries = parse_program_response(&reply);
    anyhow::ensure!(
        !entries.is_empty(),
        "the coach's reply held no sessions we could read"
    );

    // Resolve names before touching the calendar, so an unusable reply cannot
    // leave the rider with a hole where their plan was.
    let start = next_monday(today);
    let mut to_schedule: Vec<(i64, String)> = Vec::new();
    for entry in &entries {
        let date = start
            + CDuration::days(
                (entry.week.max(1) as i64 - 1) * 7 + day_name_to_offset(&entry.day) as i64,
            );
        match library
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, &entry.workout_name))
        {
            Some(w) => to_schedule.push((w.id, date.format("%Y-%m-%d").to_string())),
            None => tracing::warn!("Workout '{}' not in library — skipped", entry.workout_name),
        }
    }
    anyhow::ensure!(
        !to_schedule.is_empty(),
        "none of the coach's sessions matched a workout in your library"
    );

    db::clear_future_sessions(&pool, program_id, start).await?;
    let mut written = 0usize;
    for (workout_id, date) in to_schedule {
        match db::schedule_workout(&pool, workout_id, &date, Some(program_id)).await {
            Ok(_) => written += 1,
            Err(e) => tracing::error!("scheduling {workout_id} on {date}: {e}"),
        }
    }
    Ok(written)
}

/// A short label for how long ago a date was, for the missed-session line.
fn days_ago(date: NaiveDate, today: NaiveDate) -> String {
    match (today - date).num_days() {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n if n < 7 => format!("{n} days ago"),
        n => format!("{} weeks ago", n / 7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::WorkoutCategory;
    use crate::training::program::{status, PlannedSession, Program};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    fn program() -> Program {
        Program {
            id: 1,
            start_monday: date(2026, 8, 3),
            num_weeks: 12,
            training_days: "monday".into(),
        }
    }

    fn session(id: i64, d: NaiveDate, completed: bool) -> PlannedSession {
        PlannedSession {
            entry_id: id,
            date: d,
            workout_id: id,
            workout_name: "Threshold".into(),
            category: WorkoutCategory::Threshold,
            tss: 60.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
        }
    }

    #[test]
    fn should_describe_the_week_and_phase() {
        let state = status(&program(), &[], date(2026, 8, 12));
        assert_eq!(PlanCard::describe(&state), "Week 2 of 12 · Build");
    }

    #[test]
    fn should_name_a_recovery_week_in_the_description() {
        let state = status(&program(), &[], date(2026, 8, 26));
        assert_eq!(PlanCard::describe(&state), "Week 4 of 12 · Recovery");
    }

    #[test]
    fn should_explain_a_recovery_week_rather_than_showing_nothing() {
        let state = status(&program(), &[], date(2026, 8, 26));
        let (title, _) = PlanCard::nothing_to_change(&state);
        assert_eq!(title, "Recovery week — nothing to change");
    }

    #[test]
    fn should_say_the_plan_fits_when_there_is_work_left_and_no_reason_to_change() {
        let sessions = vec![session(1, date(2026, 8, 14), false)];
        let state = status(&program(), &sessions, date(2026, 8, 12));
        let (title, _) = PlanCard::nothing_to_change(&state);
        assert_eq!(title, "The plan still fits");
    }

    #[test]
    fn should_say_so_when_the_plan_has_run_out() {
        let sessions = vec![session(1, date(2026, 8, 5), true)];
        let state = status(&program(), &sessions, date(2026, 8, 12));
        let (title, _) = PlanCard::nothing_to_change(&state);
        assert_eq!(title, "No sessions left to adjust");
    }

    #[test]
    fn should_promise_not_to_claw_back_missed_work() {
        // A rider who has missed sessions must be told the plan is not
        // silently adding them back somewhere.
        let sessions = vec![
            session(1, date(2026, 8, 5), false),
            session(2, date(2026, 8, 14), false),
        ];
        let state = status(&program(), &sessions, date(2026, 8, 12));
        let (_, subtitle) = PlanCard::nothing_to_change(&state);
        assert!(subtitle.contains("Nothing is being added to catch up"));
    }

    #[test]
    fn should_phrase_how_long_ago_a_session_was() {
        let today = date(2026, 8, 12);
        assert_eq!(days_ago(today, today), "today");
        assert_eq!(days_ago(date(2026, 8, 11), today), "yesterday");
        assert_eq!(days_ago(date(2026, 8, 9), today), "3 days ago");
        assert_eq!(days_ago(date(2026, 7, 29), today), "2 weeks ago");
    }
}
