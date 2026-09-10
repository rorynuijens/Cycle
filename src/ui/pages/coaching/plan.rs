//! "Your Program" — the plan the rider is living with, and what should change
//! about it.
//!
//! The card has four faces, and shows exactly one:
//!
//! * a program is being followed → where it has got to, what was missed, and
//!   any adjustment the rules propose;
//! * a program that has run out → what the block delivered, and the offer to
//!   start the next one locally;
//! * no program, but the calendar holds scheduled workouts that belong to none
//!   → an offer to adopt them, so a rider who plainly has a plan is not told
//!   they have none;
//! * neither → nothing at all. The Build Program section below is the answer.
//!
//! The adjustments come from [`crate::training::program`], which is pure and
//! costs nothing to run. Only "Rebuild with AI" spends the rider's key.

use adw::glib;
use adw::prelude::*;
use chrono::{Datelike, Duration as CDuration, Local, NaiveDate};
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, db, workout::Workout};
use crate::training::fitness::TsbBand;
use crate::training::program::{
    block_summary, last_day, plan_view, Adjustment, BlockSummary, CoachVerdict, Phase,
    ProgramStatus,
};
use crate::training::rollover;

use super::data::{load_plan_data, PlanData};
use super::program::week_start;

/// How many missed sessions the card offers to close, at most.
///
/// The window in [`crate::training::program`] already keeps this to a fortnight;
/// this is the second bound, for the fortnight that went badly. The summary row
/// above them states the true count, so nothing is hidden — the rows are the
/// quick way to close the recent ones, not an inventory.
const MISSED_ROWS_SHOWN: usize = 5;

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
    rollover_btn: gtk::Button,
    actions: gtk::Box,
    /// What "Apply Adjustments" will write, as of the last reload.
    pending: Rc<RefCell<Vec<Adjustment>>>,
    /// The program on screen, so the action buttons know what they act on.
    program_id: Rc<RefCell<Option<i64>>>,
    /// The state the card was last drawn from, which the AI rebuild describes
    /// to the coach rather than reading the whole plan a second time.
    last_state: Rc<RefCell<Option<ProgramStatus>>>,
    /// The program, its sessions, the days really trained and the days away, as
    /// of the last reload — everything the roll-over needs to build the next
    /// block without going back to the database on the main thread.
    last_program: Rc<RefCell<Option<crate::training::program::Program>>>,
    last_sessions: Rc<RefCell<Vec<crate::training::program::PlannedSession>>>,
    trained: Rc<RefCell<std::collections::HashSet<NaiveDate>>>,
    time_off: Rc<RefCell<Vec<NaiveDate>>>,
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

/// The badge line for an adjusted session: where the plan started, and where
/// one Undo lands when that is somewhere else.
///
/// After a single adjustment those are the same workout, and the line says it
/// once.
///
/// Worded neutrally because the stored state does not record which way the
/// session moved — only the name it moved from. Now that the program can step a
/// session *up* as well as down, "Eased from" was wrong half the time it
/// mattered. Saying "Adjusted from" costs the common case a little precision
/// and never states the opposite of what happened.
fn easing_subtitle(original: &str, previous_step: Option<&str>) -> String {
    match previous_step {
        Some(step) if step != original => format!("Adjusted from {original} · back to {step}"),
        _ => format!("Adjusted from {original}"),
    }
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
        let rollover_btn = gtk::Button::builder()
            .label("Start the Next Block")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text(
                "Build the next four weeks from your own workout library. \
                 Local rules only — nothing is sent to your AI provider.",
            )
            .visible(false)
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
        actions.append(&rollover_btn);
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
            rollover_btn,
            actions,
            pending: Rc::new(RefCell::new(Vec::new())),
            program_id: Rc::new(RefCell::new(None)),
            last_state: Rc::new(RefCell::new(None)),
            last_program: Rc::new(RefCell::new(None)),
            last_sessions: Rc::new(RefCell::new(Vec::new())),
            trained: Rc::new(RefCell::new(std::collections::HashSet::new())),
            time_off: Rc::new(RefCell::new(Vec::new())),
            verdict: Rc::new(std::cell::Cell::new(CoachVerdict::Proceed)),
            athlete,
            workouts,
            pool,
            rt_handle,
            on_toast,
        });

        card.connect_apply();
        card.connect_rollover();
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
        *self.last_program.borrow_mut() = data.program.clone();
        *self.last_sessions.borrow_mut() = data.sessions.clone();
        *self.trained.borrow_mut() = data.trained.clone();
        *self.time_off.borrow_mut() = data.time_off.clone();
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
            &data.trained,
            &data.metrics,
            &data.wellness,
            &self.workouts,
            today,
            self.verdict.get(),
        );

        // One suggested action at a time: once the plan is over, starting the
        // next block is the thing to do and there is nothing left to adjust.
        self.apply_btn.set_visible(!state.over);
        self.rollover_btn.set_visible(state.over);

        self.group
            .set_description(Some(&Self::describe(&state, last_day(&program))));
        if state.over {
            self.render_finished(&block_summary(&program, &data.sessions, &data.pmc));
        } else {
            self.render_progress(&state, data.metrics.tsb(), today);
            self.render_adjustments(&adjustments, &state);
        }

        *self.pending.borrow_mut() = adjustments;
        self.apply_btn
            .set_sensitive(!self.pending.borrow().is_empty());
        *self.last_state.borrow_mut() = Some(state);
        self.root.set_visible(true);
    }

    /// "Week 3 of 12 · Build", or the date the plan ran out.
    ///
    /// A finished program used to go on reporting "Week 15 of 15 · Build" for
    /// ever, because [`crate::training::program::week_of`] clamps into the
    /// span. The clamp is right — a past session does belong to the final week
    /// — so the end is said here instead of unpicking it there.
    fn describe(state: &ProgramStatus, ends: NaiveDate) -> String {
        if state.over {
            return format!(
                "{} weeks · finished {}",
                state.total_weeks,
                ends.format("%-d %B")
            );
        }
        format!(
            "Week {} of {} · {}",
            state.week,
            state.total_weeks,
            state.phase.label()
        )
    }

    /// What to say about a block that is done with.
    ///
    /// Pure and separate from the row, like [`Self::missed_summary`]: this is
    /// the sentence that closes off fifteen weeks of a rider's training and it
    /// should be testable without a display.
    fn finished_summary(s: &BlockSummary) -> (String, String) {
        let title = format!(
            "{} weeks done — {} of {} session{} completed",
            s.weeks,
            s.completed,
            s.planned,
            if s.planned == 1 { "" } else { "s" }
        );
        let ridden = format!("{:.0} TSS from the sessions you rode as written.", s.tss);
        // Fitness needs a reading at both ends to be a change rather than a
        // number; a program older than the recorded history has neither.
        let subtitle = if s.ctl_start > 0.0 || s.ctl_end > 0.0 {
            format!(
                "{ridden} Fitness {:.0} → {:.0}.",
                s.ctl_start.round(),
                s.ctl_end.round()
            )
        } else {
            ridden
        };
        (title, subtitle)
    }

    /// The face shown once the plan has run out.
    fn render_finished(self: &Rc<Self>, summary: &BlockSummary) {
        let (title, subtitle) = Self::finished_summary(summary);
        self.add_row(
            adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .subtitle_lines(3)
                .build(),
        );
    }

    /// What to say about sessions the rider did not ride, or `None` when there
    /// is nothing to say.
    ///
    /// Reads [`ProgramStatus::missed_recent`], not `missed`: a program adopted
    /// with a start date in the past carries sessions that were never rideable,
    /// and a count including those can never come down however well the rider
    /// trains. Pure and separate from the row it fills so the wording — the most
    /// rider-visible sentence the card says — can be tested without GTK.
    fn missed_summary(state: &ProgramStatus, today: NaiveDate) -> Option<(String, String)> {
        let last = state.missed_recent.last()?.date;
        let count = state.missed_recent.len();
        Some((
            format!(
                "{count} session{} missed in the last fortnight",
                if count == 1 { "" } else { "s" }
            ),
            // Saying so plainly, because the plan will not try to claw them
            // back and the rider should know that is deliberate.
            format!(
                "Most recently {} ({}). Missed sessions are not rescheduled — \
                 the plan carries on from here.",
                last.format("%-d %B"),
                days_ago(last, today)
            ),
        ))
    }

    fn render_progress(self: &Rc<Self>, state: &ProgramStatus, tsb: f64, today: NaiveDate) {
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

        if let Some((title, subtitle)) = Self::missed_summary(state, today) {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .subtitle_lines(3)
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("dialog-warning-symbolic")
                    .css_classes(["warning"])
                    .build(),
            );
            self.add_row(row);
            self.render_missed_sessions(state, today);
        }
    }

    /// The missed sessions themselves, each with a way to close it.
    ///
    /// Most of this rider's training happens outdoors, so a session the program
    /// calls missed was often ridden — just not here. Without these rows the
    /// card states a problem and offers no way out of it, and the count above
    /// stays wrong for a fortnight whatever the rider does.
    ///
    /// Most recent first, and capped: the summary row above carries the true
    /// count, so a bad fortnight does not push the adjustments off the card.
    fn render_missed_sessions(self: &Rc<Self>, state: &ProgramStatus, today: NaiveDate) {
        for session in state.missed_recent.iter().rev().take(MISSED_ROWS_SHOWN) {
            let row = adw::ActionRow::builder()
                .title(format!(
                    "{}  {}",
                    session.date.format("%a %-d %b"),
                    session.workout_name
                ))
                .subtitle(days_ago(session.date, today))
                .build();
            let icon = gtk::Image::builder()
                .icon_name("media-playlist-consecutive-symbolic")
                .css_classes(["dim-label"])
                .build();
            icon.update_property(&[gtk::accessible::Property::Label("Not done yet")]);
            row.add_prefix(&icon);

            let done = gtk::Button::builder()
                .label("Mark done")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .tooltip_text("Mark this session done without riding it here")
                .build();

            // Weak, not strong: this row is rebuilt on every reload, and a
            // strong capture would put the card inside a widget the card owns
            // (CLAUDE.md §2.4).
            let card = Rc::downgrade(self);
            let entry_id = session.entry_id;
            done.connect_clicked(move |_| {
                let Some(card) = card.upgrade() else { return };
                card.set_done(entry_id, true);
            });
            row.add_suffix(&done);

            self.add_row(row);
        }
    }

    /// Settle a missed session against the plan.
    ///
    /// Goes through the calendar's shared action so the toast, the logging and
    /// the reload read the same here as they do on the week list and in the day
    /// detail dialog. Only one direction from this card: the way back lives
    /// beside the session on the calendar, where the rider can see the day.
    fn set_done(self: &Rc<Self>, entry_id: i64, done: bool) {
        let card = Rc::clone(self);
        // Strong, and deliberately so: this closure is held by the pending task
        // and dropped with it, never by a widget the card owns.
        let reload: Rc<dyn Fn()> = Rc::new(move || card.reload());

        crate::ui::pages::calendar::actions::set_session_done(
            self.pool.clone(),
            &self.rt_handle.clone(),
            entry_id,
            done,
            Rc::clone(&self.on_toast),
            reload,
            // The reload rebuilds the card's rows from the database.
            None,
        );
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
                .subtitle(easing_subtitle(
                    &original,
                    session.previous_step_name.as_deref(),
                ))
                .build();
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name("object-select-symbolic")
                    .css_classes(["success"])
                    .build(),
            );

            // One press, one rung. Where it lands is said in the subtitle, not
            // on the button: this row's title is already the session's own name
            // and date, and a button carrying a second workout name truncates.
            let back_to = session
                .previous_step_name
                .clone()
                .unwrap_or_else(|| original.clone());
            let undo = gtk::Button::builder()
                .label("Undo")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .tooltip_text(format!("Put {back_to} back on this day"))
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
        // The recent ones only: a rider being told "you have missed sessions"
        // about a fortnight they trained through would rightly ignore the card.
        if !state.missed_recent.is_empty() {
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
        self.apply_btn.set_visible(true);
        self.apply_btn.set_sensitive(false);
        self.rollover_btn.set_visible(false);
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

    /// Start the next block from the rider's own library, billing nothing.
    ///
    /// The counterpart to "Rebuild with AI", and the one offered first once a
    /// plan runs out: a rider whose program has ended should not have to pay a
    /// provider to carry on training.
    fn connect_rollover(self: &Rc<Self>) {
        let card = Rc::clone(self);
        self.rollover_btn.connect_clicked(move |btn| {
            let (Some(program), Some(state)) = (
                card.last_program.borrow().clone(),
                card.last_state.borrow().clone(),
            ) else {
                return;
            };
            card.present_rollover_dialog(btn, program, state);
        });
    }

    /// Ask which days, then write the block.
    ///
    /// Everything `!Send` — the toggles, the library behind the `Rc` — is read
    /// on the main thread before the write is spawned, the same shape
    /// [`Self::connect_rebuild`] uses.
    fn present_rollover_dialog(
        self: &Rc<Self>,
        anchor: &gtk::Button,
        program: crate::training::program::Program,
        state: ProgramStatus,
    ) {
        let today = Local::now().date_naive();
        let guess = self.likely_days(&state);

        let time_off: std::collections::HashSet<NaiveDate> =
            self.time_off.borrow().iter().copied().collect();
        let days_for_start = if guess.is_empty() {
            crate::ui::widgets::day_toggles::DEFAULT_DAYS.to_vec()
        } else {
            guess.clone()
        };
        let start = match rollover::next_block_monday(&program, today, &days_for_start, &time_off) {
            Ok(d) => d,
            Err(e) => {
                (self.on_toast)(
                    adw::Toast::builder()
                        .title(e.to_string())
                        .timeout(6)
                        .build(),
                );
                return;
            }
        };

        let easy_first = rollover::opens_easy(&program, start, &time_off);
        let dialog = adw::AlertDialog::new(
            Some("Start the next block"),
            Some(&Self::rollover_body(
                start,
                easy_first,
                &time_off,
                &days_for_start,
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("start", "Start Block");
        dialog.set_response_appearance("start", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("start"));
        dialog.set_close_response("cancel");

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        content.append(
            &gtk::Label::builder()
                .label("Training days")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );
        let toggles = crate::ui::widgets::day_toggles::DayToggles::new(&days_for_start);
        content.append(toggles.widget());
        dialog.set_extra_child(Some(&content));

        // Weak, so the handler on a widget inside the dialog does not own it
        // (CLAUDE.md §2.4).
        toggles.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |n| dialog.set_response_enabled("start", n > 0)
        ));

        let card = Rc::clone(self);
        let toggles_for_response = toggles.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "start" {
                return;
            }
            let days = toggles_for_response.selected();
            if days.is_empty() {
                return;
            }
            let training_days = toggles_for_response.selected_csv();
            card.write_next_block(program.clone(), days, training_days, start, easy_first);
        });

        dialog.present(Some(anchor));
    }

    /// What the dialog says before the rider agrees to it.
    ///
    /// Names the cost (none), the shape, and — because it is the one surprise
    /// in this flow — any session the opening week will lose to time off.
    fn rollover_body(
        start: NaiveDate,
        easy_first: bool,
        time_off: &std::collections::HashSet<NaiveDate>,
        days: &[chrono::Weekday],
    ) -> String {
        let shape = if easy_first {
            "an easy week first, then three building ones"
        } else {
            "three building weeks, then an easy one"
        };
        let mut body = format!(
            "Four weeks from {}, built from your own workout library — {shape}. \
             Nothing is sent to your AI provider.",
            start.format("%A %-d %B")
        );
        let lost: Vec<String> = days
            .iter()
            .map(|d| start + CDuration::days(d.num_days_from_monday() as i64))
            .filter(|d| time_off.contains(d))
            .map(|d| d.format("%A %-d %B").to_string())
            .collect();
        if !lost.is_empty() {
            body.push_str(&format!(
                "\n\n{} falls in your planned time off, so that session is skipped.",
                lost.join(" and ")
            ));
        }
        body
    }

    /// The days to tick before the rider is asked.
    ///
    /// The days they have actually *ridden* recently, not the days the plan put
    /// sessions on: fifteen weeks of dragging sessions around leaves the plan's
    /// own weekdays saying nothing — the live block's entries touch all seven,
    /// and offering that back would propose training daily.
    fn likely_days(&self, state: &ProgramStatus) -> Vec<chrono::Weekday> {
        let today = Local::now().date_naive();
        let since = today - CDuration::days(28);
        let mut days: Vec<chrono::Weekday> = self
            .trained
            .borrow()
            .iter()
            .filter(|d| **d >= since && **d <= today)
            .map(|d| d.weekday())
            .collect();
        days.sort_by_key(|d| d.num_days_from_monday());
        days.dedup();
        if days.len() < 2 {
            // One ride, or none, is not a pattern. Offer the standard week and
            // let the rider say otherwise.
            return crate::ui::widgets::day_toggles::DEFAULT_DAYS.to_vec();
        }
        let _ = state;
        days
    }

    /// Write the block, or write nothing at all.
    fn write_next_block(
        self: &Rc<Self>,
        program: crate::training::program::Program,
        days: Vec<chrono::Weekday>,
        training_days: String,
        start: NaiveDate,
        easy_first: bool,
    ) {
        let pool = self.pool.clone();
        let on_toast = Rc::clone(&self.on_toast);
        let card = Rc::clone(self);
        let library: Vec<Workout> = (*self.workouts).clone();
        let ftp = self.athlete.borrow().ftp_watts;
        let sessions = card.last_sessions.borrow().clone();

        crate::ui::spawn_to_main(
            &self.rt_handle.clone(),
            async move {
                roll_over(
                    &pool,
                    &program,
                    &sessions,
                    &library,
                    days,
                    &training_days,
                    start,
                    easy_first,
                    ftp,
                )
                .await
            },
            move |result| {
                match result {
                    Ok(written) => {
                        on_toast(
                            adw::Toast::builder()
                                .title(written.message())
                                .timeout(6)
                                .build(),
                        );
                        card.reload();
                    }
                    Err(e) => {
                        tracing::error!("starting the next block: {e}");
                        on_toast(
                            adw::Toast::builder()
                                .title("Could not start the next block")
                                .timeout(5)
                                .build(),
                        );
                    }
                };
            },
        );
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
                        // Said out loud: a rider who booked a holiday should see
                        // the plan respecting it, not just a smaller number.
                        Ok((count, off)) if off > 0 => format!(
                            "Remaining weeks replanned — {count} sessions · \
                             {off} skipped for time off"
                        ),
                        Ok((count, _)) => format!("Remaining weeks replanned — {count} sessions"),
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

/// What a roll-over managed to write.
enum Written {
    /// The block is on the calendar. `dropped` fell on time off; `thin` names
    /// the weeks that got no session the FTP check-in will ever count.
    Block {
        sessions: usize,
        dropped: usize,
        thin: usize,
    },
    /// Nothing was written, and the old program is still the rider's.
    Nothing,
}

impl Written {
    fn message(&self) -> String {
        match self {
            Self::Nothing => {
                "Nothing scheduled — every session fell on your planned time off".into()
            }
            Self::Block {
                sessions,
                dropped,
                thin,
            } => {
                let mut m = format!("Next block started — {sessions} sessions");
                if *dropped > 0 {
                    m.push_str(&format!(" · {dropped} skipped for time off"));
                }
                if *thin > 0 {
                    // Said out loud: a block with no session hard enough to
                    // measure is the whole reason the FTP check-in stays dark,
                    // and a rider would otherwise never learn why.
                    m.push_str(" · no workout in your library is hard enough to test your FTP");
                }
                m
            }
        }
    }
}

/// Build and write the next block, or write nothing at all.
///
/// Runs entirely on the tokio runtime. The order matters: the new program is
/// written and populated *before* the old one is stood down, so a failure part
/// way leaves the rider with a working plan rather than with none.
#[allow(clippy::too_many_arguments)]
async fn roll_over(
    pool: &SqlitePool,
    program: &crate::training::program::Program,
    sessions: &[crate::training::program::PlannedSession],
    library: &[Workout],
    days: Vec<chrono::Weekday>,
    training_days: &str,
    start: NaiveDate,
    easy_first: bool,
    ftp: u32,
) -> anyhow::Result<Written> {
    let block = rollover::next_block(sessions, library, start, &days, ftp, easy_first)?;

    // Re-read rather than trusting what the card was drawn from: the rider may
    // have booked a trip between opening the dialog and pressing the button,
    // and this is the check that actually holds (the dialog's warning is only
    // a courtesy).
    let last = start + CDuration::days(block.weeks as i64 * 7 - 1);
    let off: std::collections::HashSet<NaiveDate> = db::load_time_off_between(
        pool,
        &start.format("%Y-%m-%d").to_string(),
        &last.format("%Y-%m-%d").to_string(),
    )
    .await?
    .into_iter()
    .map(|t| t.date)
    .collect();

    let mut planned = block.sessions;
    let dropped = crate::ai::context::drop_time_off_days(&mut planned, &off);
    if planned.is_empty() {
        // Nothing has been written yet, so nothing is lost — the rider keeps
        // the program they had rather than having it retired and replaced by an
        // empty one.
        return Ok(Written::Nothing);
    }

    let new_id = db::save_program(pool, start, block.weeks, training_days).await?;
    let mut written = 0usize;
    for (workout_id, date) in &planned {
        match db::schedule_workout(
            pool,
            *workout_id,
            &date.format("%Y-%m-%d").to_string(),
            Some(new_id),
        )
        .await
        {
            Ok(_) => written += 1,
            Err(e) => tracing::error!("scheduling {workout_id} on {date}: {e}"),
        }
    }
    if written == 0 {
        // The only point at which an empty program can exist. Stand it back
        // down rather than leaving the rider following nothing.
        db::deactivate_program(pool, new_id).await?;
        return Ok(Written::Nothing);
    }

    // The old plan may have sessions of its own past this Monday — a rebuild
    // writes beyond the span it claims — and two plans on the same days is not
    // a calendar anybody can read.
    db::clear_future_sessions(pool, program.id, start).await?;
    db::deactivate_program(pool, program.id).await?;

    Ok(Written::Block {
        sessions: written,
        dropped,
        thin: block.weeks_without_hard_evidence.len(),
    })
}

/// Whole weeks from `start` through `end`, and never fewer than a block.
///
/// The floor is what makes a replan of a *finished* program mean something: its
/// span has run out, so the honest answer is zero, and asking the coach for zero
/// weeks would return nothing at all. A block is the smallest plan worth
/// building.
fn weeks_between(start: NaiveDate, end: NaiveDate) -> u32 {
    let days = (end - start).num_days();
    if days < 0 {
        return crate::training::program::BLOCK_WEEKS;
    }
    ((days / 7) as u32 + 1).max(crate::training::program::BLOCK_WEEKS)
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
) -> anyhow::Result<(usize, usize)> {
    use crate::ai::coach::{
        build_program_revision_prompt, get_suggestion, parse_program_response,
        ProgramRevisionContext,
    };
    use crate::ai::context::{
        drop_time_off_days, entry_date, wellness_snapshots, workouts_as_options,
    };

    // Week 1 of the reply is this Monday, and the coach is told so — a
    // (week, day) answer can only dodge a date if the dates are pinned first.
    let start = next_monday(today);
    let program = db::active_program(&pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the program ended while it was being replanned"))?;

    // Read off the program's span, not off `state.week`. `week_of` clamps into
    // the span, so `total_weeks - week` was *always one* on a program the rider
    // had run past — the coach was asked to replan a single week on exactly the
    // programs most in need of replanning, and the same number clipped the
    // time-off window below, hiding the rider's holidays from it too.
    let weeks_left = weeks_between(start, last_day(&program));
    let data = super::data::load_program_prompt_data(
        &pool,
        today,
        start + CDuration::days(weeks_left as i64 * 7),
    )
    .await?;

    let metrics = crate::training::fitness::compute_load_metrics(
        &data.records,
        &data.intervals_pairs,
        profile.ftp_watts,
        today,
    );

    // The training days the program was built around; a program adopted from a
    // bare calendar has none recorded, so the days it actually uses stand in.
    //
    // The one place that reads the *whole* missed history rather than the recent
    // fortnight, and it must stay that way: this is inferring which weekdays the
    // rider trains on, and a fortnight of dates would hand the coach a training
    // week built from two data points.
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

    // The fortnight, not the whole history: the coach is being asked what to do
    // next, and a list reaching back to sessions that predate the program would
    // have it plan around a rider who stopped training months ago.
    let recent_missed: Vec<String> = state
        .missed_recent
        .iter()
        .rev()
        .take(6)
        .map(|s| format!("{} — {}", s.date.format("%a %-d %b"), s.workout_name))
        .collect();

    let off_days: std::collections::HashSet<NaiveDate> =
        data.time_off.iter().map(|t| t.date).collect();

    let ctx = ProgramRevisionContext {
        athlete: profile,
        ctl: metrics.ctl,
        tsb: metrics.tsb(),
        goals: data.goals,
        athlete_context: data.athlete_ctx,
        workout_options: workouts_as_options(&library, &data.icu_workouts),
        training_days,
        current_week: state.week,
        // The same count the time-off window above was read for: the prompt
        // clips time off to the weeks it is planning, so a second expression
        // here could silently load a date the prompt then throws away.
        weeks_remaining: weeks_left,
        completed: state.completed,
        missed: state.missed_recent.len(),
        recent_missed,
        wellness: wellness_snapshots(&data.wellness),
        start_monday: start,
        time_off: off_days.iter().copied().collect(),
    };

    let reply = get_suggestion(&api_key, &build_program_revision_prompt(&ctx), 2800).await?;
    let entries = parse_program_response(&reply);
    anyhow::ensure!(
        !entries.is_empty(),
        "the coach's reply held no sessions we could read"
    );

    // Resolve names before touching the calendar, so an unusable reply cannot
    // leave the rider with a hole where their plan was.
    let mut to_schedule: Vec<(i64, NaiveDate)> = Vec::new();
    for entry in &entries {
        let date = entry_date(start, entry);
        match library
            .iter()
            .find(|w| crate::ai::naming::names_match(&w.name, &entry.workout_name))
        {
            Some(w) => to_schedule.push((w.id, date)),
            None => tracing::warn!("Workout '{}' not in library — skipped", entry.workout_name),
        }
    }
    anyhow::ensure!(
        !to_schedule.is_empty(),
        "none of the coach's sessions matched a workout in your library"
    );

    // The prompt asks for this; this enforces it.
    let dropped = drop_time_off_days(&mut to_schedule, &off_days);
    anyhow::ensure!(
        !to_schedule.is_empty(),
        "every session the coach returned fell on a day you are away"
    );

    db::clear_future_sessions(&pool, program_id, start).await?;
    let mut written = 0usize;
    for (workout_id, date) in to_schedule {
        let date = date.format("%Y-%m-%d").to_string();
        match db::schedule_workout(&pool, workout_id, &date, Some(program_id)).await {
            Ok(_) => written += 1,
            Err(e) => tracing::error!("scheduling {workout_id} on {date}: {e}"),
        }
    }

    // The plan now reaches further than the row says it does. Without this the
    // program goes on claiming a span its own sessions sit outside, `week_of`
    // stays pinned to the final week, and a rebuild can never move a finished
    // program off "Week 15 of 15" however many times it is run.
    let reaches = start + CDuration::days(weeks_left as i64 * 7 - 1);
    let span = weeks_between(program.start_monday, reaches);
    db::update_program_span(&pool, program_id, span).await?;

    Ok((written, dropped))
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
            trained: false,
            entry_id: id,
            date: d,
            workout_id: id,
            workout_name: "Threshold".into(),
            category: WorkoutCategory::Threshold,
            tss: 60.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    #[test]
    fn should_describe_the_week_and_phase() {
        let state = status(&program(), &[], date(2026, 8, 12));
        let ends = last_day(&program());
        assert_eq!(PlanCard::describe(&state, ends), "Week 2 of 12 · Build");
    }

    #[test]
    fn should_name_a_recovery_week_in_the_description() {
        let state = status(&program(), &[], date(2026, 8, 26));
        let ends = last_day(&program());
        assert_eq!(PlanCard::describe(&state, ends), "Week 4 of 12 · Recovery");
    }

    #[test]
    fn should_say_the_plan_has_finished_instead_of_freezing_on_the_final_week() {
        // The bug this release exists for: a program the rider has run past
        // used to describe itself as "Week 12 of 12" for ever.
        let ends = last_day(&program());
        let state = status(&program(), &[], ends + CDuration::days(1));
        assert!(state.over);
        assert_eq!(
            PlanCard::describe(&state, ends),
            "12 weeks · finished 25 October"
        );
    }

    #[test]
    fn should_name_what_the_block_delivered() {
        let summary = BlockSummary {
            weeks: 15,
            completed: 5,
            planned: 26,
            tss: 1840.0,
            ctl_start: 42.0,
            ctl_end: 51.0,
        };
        let (title, subtitle) = PlanCard::finished_summary(&summary);
        assert_eq!(title, "15 weeks done — 5 of 26 sessions completed");
        assert_eq!(
            subtitle,
            "1840 TSS from the sessions you rode as written. Fitness 42 → 51."
        );
    }

    #[test]
    fn should_leave_the_fitness_out_when_there_is_no_ride_history_to_read_it_from() {
        let summary = BlockSummary {
            weeks: 4,
            completed: 0,
            planned: 12,
            tss: 0.0,
            ctl_start: 0.0,
            ctl_end: 0.0,
        };
        let (_, subtitle) = PlanCard::finished_summary(&summary);
        assert_eq!(subtitle, "0 TSS from the sessions you rode as written.");
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
    fn should_count_only_recent_missed_sessions_in_the_missed_row() {
        // The live case this release exists for: a program adopted with a start
        // date in the past carries sessions that were never rideable, and the
        // card used to count them forever.
        let today = date(2026, 8, 12);
        let sessions = vec![
            session(1, date(2026, 6, 16), false), // predates the program
            session(2, date(2026, 8, 5), false),
            session(3, date(2026, 8, 10), false),
        ];
        let state = status(&program(), &sessions, today);

        assert_eq!(state.missed.len(), 3, "the full history is still there");
        let (title, subtitle) =
            PlanCard::missed_summary(&state, today).expect("two were missed recently");
        assert_eq!(title, "2 sessions missed in the last fortnight");
        assert!(subtitle.contains("Most recently 10 August (2 days ago)"));
    }

    #[test]
    fn should_say_session_singular_when_only_one_was_missed_recently() {
        let today = date(2026, 8, 12);
        let sessions = vec![session(1, date(2026, 8, 10), false)];
        let state = status(&program(), &sessions, today);

        let (title, _) = PlanCard::missed_summary(&state, today).expect("one was missed");
        assert_eq!(title, "1 session missed in the last fortnight");
    }

    #[test]
    fn should_not_claim_missed_work_when_the_last_miss_was_a_month_ago() {
        // The companion to `should_promise_not_to_claw_back_missed_work`, which
        // survives the window only because its miss is seven days back. Nothing
        // recent was missed here, so the card must say neither the missed row
        // nor the "you have missed sessions" verdict.
        let today = date(2026, 8, 12);
        let sessions = vec![
            session(1, date(2026, 7, 12), false), // a month ago
            session(2, date(2026, 8, 14), false), // still to come
        ];
        let state = status(&program(), &sessions, today);

        assert!(PlanCard::missed_summary(&state, today).is_none());
        let (_, subtitle) = PlanCard::nothing_to_change(&state);
        assert!(subtitle.contains("ride it as written"), "got: {subtitle}");
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

    #[test]
    fn should_name_the_step_below_in_the_badge_line() {
        assert_eq!(
            easing_subtitle("Endurance 60", Some("Recovery Ride")),
            "Adjusted from Endurance 60 · back to Recovery Ride"
        );
    }

    #[test]
    fn should_say_the_workout_once_when_one_press_goes_home() {
        assert_eq!(
            easing_subtitle("Endurance 60", Some("Endurance 60")),
            "Adjusted from Endurance 60"
        );
        assert_eq!(
            easing_subtitle("Endurance 60", None),
            "Adjusted from Endurance 60"
        );
    }
}

/// Offscreen renders of the end-of-program card, for reviewing it without a
/// screen. See [`crate::ui::pages::calendar::screenshots`] for why this exists
/// and how to run it.
#[cfg(test)]
mod shots {
    use super::*;
    use crate::data::workout::{Segment, WorkoutCategory};
    use crate::training::program::{PlannedSession, Program};

    use crate::ui::pages::calendar::screenshots::{shoot, start, theme};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    /// The rider's own program, as it will stand on 28 September.
    fn program() -> Program {
        Program {
            id: 1,
            start_monday: date(2026, 6, 15),
            num_weeks: 15,
            training_days: String::new(),
        }
    }

    fn session(d: NaiveDate, name: &str, cat: WorkoutCategory, completed: bool) -> PlannedSession {
        PlannedSession {
            trained: false,
            entry_id: 1,
            date: d,
            workout_id: 1,
            workout_name: name.into(),
            category: cat,
            tss: 44.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    fn library() -> Vec<Workout> {
        vec![Workout {
            id: 1,
            name: "Threshold 3x10".into(),
            description: String::new(),
            duration_secs: 3600,
            tss: 70.0,
            category: WorkoutCategory::Threshold,
            segments: vec![Segment::steady(3600, 100.0, "Threshold")],
        }]
    }

    /// The card drawn from a real database, when one is named.
    ///
    /// `CYCLE_SHOT_DB=/path/to/cycle.db` renders the card through the same
    /// `load_plan_data` the app uses, against a copy of real training history.
    /// The fixture below is the fallback; this is what proves the page.
    fn card_window_from_db(
        rt: &tokio::runtime::Runtime,
        path: &str,
        today: NaiveDate,
    ) -> adw::Window {
        let (pool, data) = rt
            .block_on(async {
                let pool = sqlx::SqlitePool::connect(&format!("sqlite://{path}")).await?;
                let data = super::super::data::load_plan_data(&pool, today, 200).await?;
                Ok::<_, anyhow::Error>((pool, data))
            })
            .expect("the named database");

        let card = PlanCard::new(
            pool,
            rt.handle().clone(),
            Rc::new(RefCell::new(AthleteProfile::default())),
            Rc::new(library()),
            Rc::new(|_| {}),
        );
        card.render(data, today);
        frame(card)
    }

    /// Wrap a rendered card in a window, clamped as the page clamps it.
    fn frame(card: Rc<PlanCard>) -> adw::Window {
        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .child(card.widget())
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&clamp));
        let window = adw::Window::builder().title("Coaching").build();
        window.set_content(Some(&view));
        // The card owns callbacks the window outlives; keep it alive for the shot.
        unsafe { window.set_data("plan-card", card) };
        window
    }

    /// The card as a named database really renders it.
    ///
    /// ```text
    /// CYCLE_SHOT_DB=~/cycle-rollover-test/cycle.db \
    ///   cargo test -- --ignored --test-threads=1 shot_plan_card_live
    /// ```
    #[test]
    #[ignore]
    fn shot_plan_card_live() {
        let Ok(path) = std::env::var("CYCLE_SHOT_DB") else {
            println!("CYCLE_SHOT_DB not set — nothing to render");
            return;
        };
        let name = std::env::var("CYCLE_SHOT_NAME").unwrap_or_else(|_| "plan-live".into());
        start();
        let rt = tokio::runtime::Runtime::new().expect("a runtime for the card's reads");
        let today = Local::now().date_naive();
        for (dark, suffix) in [(false, "light"), (true, "dark")] {
            theme(dark);
            let w = card_window_from_db(&rt, &path, today);
            shoot(&w, 900, 420, &format!("{name}-{suffix}"));
        }
    }

    /// The card, drawn from real-shaped data, in a window of its own.
    fn card_window(rt: &tokio::runtime::Runtime, today: NaiveDate) -> adw::Window {
        let pool = rt
            .block_on(async {
                let pool = sqlx::SqlitePool::connect(":memory:").await?;
                crate::data::migrate::run(&pool).await?;
                Ok::<_, anyhow::Error>(pool)
            })
            .expect("an empty database");

        let card = PlanCard::new(
            pool,
            rt.handle().clone(),
            Rc::new(RefCell::new(AthleteProfile::default())),
            Rc::new(library()),
            Rc::new(|_| {}),
        );

        // The live block: 26 planned, 5 ridden, over fifteen weeks.
        let mut sessions = vec![
            session(
                date(2026, 6, 16),
                "Endurance 75",
                WorkoutCategory::Endurance,
                true,
            ),
            session(date(2026, 7, 3), "2x15 Tempo", WorkoutCategory::Tempo, true),
            session(
                date(2026, 8, 7),
                "Endurance 60",
                WorkoutCategory::Endurance,
                true,
            ),
            session(
                date(2026, 8, 28),
                "2x15 Tempo",
                WorkoutCategory::Tempo,
                true,
            ),
            session(
                date(2026, 9, 7),
                "Endurance 75",
                WorkoutCategory::Endurance,
                true,
            ),
        ];
        for n in 0..21 {
            sessions.push(session(
                date(2026, 6, 17) + CDuration::days(n * 4),
                "Sweet Spot Base I",
                WorkoutCategory::SweetSpot,
                false,
            ));
        }

        let pmc = vec![
            crate::training::fitness::PmcPoint {
                date: date(2026, 6, 15),
                ctl: 11.0,
                atl: 9.0,
                tsb: 2.0,
            },
            crate::training::fitness::PmcPoint {
                date: date(2026, 9, 27),
                ctl: 16.0,
                atl: 2.0,
                tsb: 14.0,
            },
        ];

        card.render(
            PlanData {
                program: Some(program()),
                sessions,
                trained: std::collections::HashSet::new(),
                metrics: crate::training::fitness::LoadMetrics::default(),
                pmc,
                wellness: Vec::new(),
                orphans: None,
                time_off: (23..=28).map(|d| date(2026, 9, d)).collect(),
            },
            today,
        );

        frame(card)
    }

    /// Both faces, both themes, in one test.
    ///
    /// One test function, not two: GTK may only be initialised from a single
    /// thread, and the test harness gives each `#[test]` its own even at
    /// `--test-threads=1`.
    #[test]
    #[ignore]
    fn shot_plan_card() {
        start();
        let rt = tokio::runtime::Runtime::new().expect("a runtime for the card's reads");
        for (dark, suffix) in [(false, "light"), (true, "dark")] {
            theme(dark);
            // 28 September: the day after the rider's program runs out.
            let finished = card_window(&rt, date(2026, 9, 28));
            shoot(&finished, 900, 360, &format!("plan-finished-{suffix}"));
            // The ordinary face, to compare the change against.
            let running = card_window(&rt, date(2026, 9, 8));
            shoot(&running, 900, 360, &format!("plan-running-{suffix}"));
        }
    }
}
