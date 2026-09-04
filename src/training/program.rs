//! A training program the rider is actually living with.
//!
//! The plan builder produces weeks of sessions and puts them on the calendar.
//! This module is the other half: what has happened to that plan since, and
//! what — if anything — should change about the part that has not happened yet.
//!
//! Everything here is deterministic and local. The rules cost nothing to run,
//! can be tested without a network, and each one has to be defensible as
//! coaching rather than merely as code. The AI rebuild is a separate, billed
//! path for when the rider wants the whole remainder reconsidered.
//!
//! Two principles run through it:
//!
//! * **A missed session is gone.** Nothing is ever moved or stacked to catch
//!   up. Training load is a rolling average, and riding two hard days back to
//!   back to repay a debt is how people get hurt.
//! * **Easing is cheap, adding is not.** Any single warning is enough to ease a
//!   session: the worst case for easing wrongly is a slightly light day. Exactly
//!   one rule adds work, and it is held to a much higher bar — every wellness
//!   signal it can see agreeing, two mornings running, on a rider whose form is
//!   already fresh — because the worst case for adding wrongly is an injury. It
//!   will not sharpen a session that is already hard.

use chrono::NaiveDate;

use crate::data::db::WellnessEntry;
use crate::data::workout::{Workout, WorkoutCategory};
use crate::training::analytics::build_wellness_series;
use crate::training::fitness::{LoadMetrics, TsbBand};

/// Weeks in a block: three building, then one easier.
const BLOCK_WEEKS: u32 = 4;

/// Consecutive missed sessions before the next one is eased.
///
/// One missed session is life. Two in a row means the adaptation the next hard
/// session assumes was never made.
const MISSED_RUN_FOR_EASING: usize = 2;

/// How far back a missed session still counts as missed.
///
/// Without a window, `missed` only ever grows: a program adopted with a start
/// date in the past carries sessions that were never rideable, and the card ends
/// up reporting a number that can never come down. A fortnight is what a rider
/// means by "recently" — long enough that a bad week still shows, short enough
/// that last season is not held against them.
///
/// Deliberately equal to, but independent of, `analytics::WELLNESS_WINDOW_DAYS`:
/// that one is the width of a sparkline, and shortening a chart must not quietly
/// change what the coach is told.
const MISSED_RECENT_DAYS: i64 = 14;

/// How far resting heart rate must sit above its own recent average to read as
/// a body under strain rather than day-to-day variation.
///
/// Deliberately stricter than the 3 % the wellness card uses to colour a trend:
/// that threshold answers "is this worth showing you", this one answers "is
/// this worth changing your training over".
const RESTING_HR_ELEVATION: f32 = 0.05;

/// A sleep score at or below this reads as a bad night rather than a normal one.
const POOR_SLEEP_SCORE: f32 = 50.0;

/// Readings needed before a wellness baseline means anything.
const MIN_WELLNESS_READINGS: usize = 4;

/// How far *below* baseline a resting heart rate reads as genuinely recovered.
///
/// Deliberately not the mirror of [`RESTING_HR_ELEVATION`]. A resting HR 5 %
/// above baseline is a warning worth easing for; 3 % below is a real signal in
/// its own right rather than merely the absence of a warning. Evidence that
/// justifies taking work away does not automatically justify adding it.
const RESTING_HR_SUPPRESSION: f32 = 0.03;

/// A sleep score at or above this reads as a genuinely good night, rather than
/// merely not a bad one.
const GOOD_SLEEP_SCORE: f32 = 75.0;

/// How far above its baseline an HRV reading reads as recovered.
const HRV_ELEVATION: f32 = 0.05;

/// Wellness signals that must be present, and agree, for a morning to count.
///
/// One signal is not a morning. A good sleep score on its own says the watch
/// was worn, not that the rider is ready.
const SIGNALS_FOR_STRONG_MORNING: usize = 2;

/// Consecutive strong mornings before the plan will add work.
///
/// One good night is noise — a late dinner or a quiet day moves every one of
/// these numbers. Two in a row is the shortest run that is not.
const STRONG_MORNINGS_FOR_PUSH: usize = 2;

/// A program the rider is following.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub id: i64,
    pub start_monday: NaiveDate,
    pub num_weeks: u32,
    /// The days the plan was built around, as stored. Shown, not computed on.
    pub training_days: String,
}

/// One session the program put on the calendar.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedSession {
    pub entry_id: i64,
    pub date: NaiveDate,
    pub workout_id: i64,
    pub workout_name: String,
    pub category: WorkoutCategory,
    pub tss: f32,
    pub duration_secs: u32,
    /// Whether the rider marked *this planned session* done.
    pub completed: bool,
    /// Whether real training happened on this day, whatever the plan asked for.
    ///
    /// Separate from `completed` because they answer different questions. A
    /// 90-minute road ride on a day the plan wanted 30 minutes of recovery did
    /// not complete that session — but it is emphatically not a day the rider
    /// skipped, and counting it as missed is what had the program easing this
    /// rider's training for a reason that was never true.
    ///
    /// Filled in by [`mark_trained`] from
    /// [`crate::training::matching::trained_days`]; the database cannot know it,
    /// because it is a fact about rides rather than about the plan.
    pub trained: bool,
    /// The workout the program originally asked for, when this entry has
    /// already been eased.
    pub adjusted_from: Option<String>,
    /// The workout this entry held before the most recent ease — where an Undo
    /// would put it back to.
    ///
    /// Distinct from `adjusted_from`: after two eases the origin is two rungs
    /// down and Undo only walks one. They coincide after a single ease, which
    /// is why the difference went unnoticed until a session was eased twice.
    pub previous_step_name: Option<String>,
}

impl PlannedSession {
    /// Whether this planned day is settled — nothing more is expected of it.
    ///
    /// True when the session was done, and also when the day was simply trained
    /// on. Everything that asks "was this missed?" asks this instead, so the two
    /// answers can never drift apart.
    pub fn settled(&self) -> bool {
        self.completed || self.trained
    }
}

/// Note which planned days had real training on them.
///
/// Kept out of the database layer on purpose: which days were trained is a fact
/// about rides, and the plan's own tables know nothing about rides.
pub fn mark_trained(
    sessions: &[PlannedSession],
    trained: &std::collections::HashSet<NaiveDate>,
) -> Vec<PlannedSession> {
    sessions
        .iter()
        .map(|s| PlannedSession {
            trained: trained.contains(&s.date),
            ..s.clone()
        })
        .collect()
}

/// Where a week sits in the build/recover cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Build,
    Recovery,
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Recovery => "Recovery",
        }
    }
}

/// The program as it actually stands today.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramStatus {
    /// 1-based, clamped into the program's span.
    pub week: u32,
    pub total_weeks: u32,
    pub phase: Phase,
    pub planned: usize,
    pub completed: usize,
    /// Past sessions that were never ridden, oldest first.
    ///
    /// The whole history. Read this when you need every day the program has ever
    /// used — inferring training days from a bare calendar, for instance — and
    /// [`Self::missed_recent`] when you are telling the rider, or the coach, how
    /// they are doing.
    pub missed: Vec<PlannedSession>,
    /// The last [`MISSED_RECENT_DAYS`] of `missed`, oldest first.
    ///
    /// Derived from `missed` rather than filtered afresh, so the two can never
    /// disagree about what missing a session means.
    pub missed_recent: Vec<PlannedSession>,
    /// Sessions still to come, soonest first.
    pub upcoming: Vec<PlannedSession>,
}

impl ProgramStatus {
    /// How many of the most recent decided sessions were missed in a row.
    ///
    /// Counts backwards from the last session whose day has passed, so a rider
    /// who missed two a fortnight ago and has ridden everything since scores
    /// zero.
    pub fn missed_run(&self, decided: &[PlannedSession]) -> usize {
        decided.iter().rev().take_while(|s| !s.settled()).count()
    }
}

/// What the morning brief made of today.
///
/// The brief never picks the session — the program owns that, because it is the
/// only thing here with a memory longer than one morning. The brief answers the
/// one question a rule reading yesterday's numbers cannot: how today's rider
/// actually is.
///
/// It lives in this module rather than in `ai` because `training` must not
/// depend on `ai` (CLAUDE.md §2.6); `ai::brief` re-exports it.
/// Serialised with the brief it came from, so the card the rider sees after a
/// restart is the one the plan was adjusted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoachVerdict {
    /// No brief today, or it agreed with the plan.
    #[default]
    Proceed,
    /// Ride, but lighter than planned.
    Ease,
    /// Do not train today.
    Rest,
}

/// Why a session is being adjusted. Each variant carries what it was measured
/// from, so the card can show the rider the number behind the advice.
///
/// All but [`Self::Primed`] ease; that one alone adds work.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// Form is dug in.
    Fatigued { tsb: f64 },
    /// Resting heart rate is up on its own baseline, or sleep was poor.
    WellnessDip { resting_hr_pct: Option<f32> },
    /// Sessions were missed back to back.
    MissedRun { count: usize },
    /// Every wellness signal available agreed the rider is recovered, for
    /// [`STRONG_MORNINGS_FOR_PUSH`] mornings running, on already-fresh form.
    ///
    /// The only reason here that adds work rather than removing it.
    Primed { mornings: usize },
    /// The morning brief read today's signals as a day to ride lighter.
    ///
    /// Deliberately carries nothing: [`Reason`] is `Copy`, which [`suggest`]
    /// relies on, and the brief's own sentence is already on the rider's
    /// dashboard where it was written.
    CoachAdvised,
}

impl Reason {
    /// The sentence shown under the adjustment, addressed to the rider.
    pub fn text(&self) -> String {
        match self {
            Self::Fatigued { tsb } => format!(
                "Your form is {tsb:+.0}. A hard session on top of that buys fatigue, not fitness."
            ),
            Self::WellnessDip {
                resting_hr_pct: Some(pct),
            } => format!(
                "Your resting heart rate is {pct:.0}% above its recent average — \
                 a sign your body is still dealing with something."
            ),
            Self::WellnessDip {
                resting_hr_pct: None,
            } => "You slept badly. Quality work on poor sleep tends to be quality \
                  in name only."
                .to_string(),
            Self::MissedRun { count } => format!(
                "You have missed {count} sessions in a row. Easing back in beats \
                 picking up where the plan assumed you would be."
            ),
            Self::Primed { mornings } => format!(
                "Your recovery signals have all read strong for {mornings} mornings \
                 running, and your form is fresh. This is a day your body can take \
                 a little more than the plan asked for."
            ),
            Self::CoachAdvised => "Your morning brief read today's signals as a day to \
                                   ride lighter."
                .to_string(),
        }
    }

    /// Which reason wins when several apply at once.
    ///
    /// Measured fatigue outranks a wellness signal, which outranks missed work:
    /// the first is the most direct evidence of accumulated load, the last is
    /// the most easily explained by an ordinary busy week.
    ///
    /// The brief's verdict ranks below all of them, and that is not a judgement
    /// on it. This picks the *sentence shown*, not whether to ease at all: when
    /// the brief and a measurement agree, "your form is -18" tells the rider
    /// something "your coach said so" does not. It wins only when it is alone —
    /// which is exactly when it is the only thing that noticed.
    fn severity(&self) -> u8 {
        match self {
            Self::Fatigued { .. } => 3,
            Self::WellnessDip { .. } => 2,
            Self::MissedRun { .. } => 1,
            Self::CoachAdvised => 0,
            // Never ranked against the others: it is only ever produced when
            // none of them apply, because anything that eases outranks wanting
            // to add work by definition.
            Self::Primed { .. } => 0,
        }
    }
}

/// A proposed change to one upcoming session.
///
/// There is no "drop" or "move" counterpart on purpose — see the module note.
#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    pub entry_id: i64,
    pub date: NaiveDate,
    pub from_workout_id: i64,
    pub from_name: String,
    pub to_workout_id: i64,
    pub to_name: String,
    pub reason: Reason,
}

/// Which week of the program `today` falls in, 1-based and clamped to its span.
///
/// A date before the program starts reads as week 1, and one past the end reads
/// as the final week, so a plan the rider has run past still describes itself
/// rather than reporting a week that does not exist.
pub fn week_of(program: &Program, today: NaiveDate) -> u32 {
    let days = (today - program.start_monday).num_days();
    if days < 0 {
        return 1;
    }
    let week = (days / 7) as u32 + 1;
    week.min(program.num_weeks.max(1))
}

/// Whether a week builds or recovers, on the classic three-up-one-down block.
///
/// Programs shorter than a full block never reach a recovery week, which falls
/// out of the arithmetic rather than needing a case of its own.
pub fn phase_of(week: u32) -> Phase {
    if week > 0 && week.is_multiple_of(BLOCK_WEEKS) {
        Phase::Recovery
    } else {
        Phase::Build
    }
}

/// Sort the program's sessions into what has happened and what has not.
pub fn status(program: &Program, sessions: &[PlannedSession], today: NaiveDate) -> ProgramStatus {
    let mut ordered = sessions.to_vec();
    ordered.sort_by_key(|s| (s.date, s.entry_id));

    let completed = ordered.iter().filter(|s| s.completed).count();
    let missed: Vec<PlannedSession> = ordered
        .iter()
        .filter(|s| s.date < today && !s.settled())
        .cloned()
        .collect();
    let missed_recent: Vec<PlannedSession> = missed
        .iter()
        .filter(|s| (today - s.date).num_days() <= MISSED_RECENT_DAYS)
        .cloned()
        .collect();
    let upcoming: Vec<PlannedSession> = ordered
        .iter()
        .filter(|s| s.date >= today && !s.completed)
        .cloned()
        .collect();

    let week = week_of(program, today);
    ProgramStatus {
        week,
        total_weeks: program.num_weeks,
        phase: phase_of(week),
        planned: ordered.len(),
        completed,
        missed,
        missed_recent,
        upcoming,
    }
}

/// Sessions whose day has passed, oldest first — the ones with a verdict.
pub fn decided(sessions: &[PlannedSession], today: NaiveDate) -> Vec<PlannedSession> {
    let mut past: Vec<PlannedSession> = sessions
        .iter()
        .filter(|s| s.date < today)
        .cloned()
        .collect();
    past.sort_by_key(|s| (s.date, s.entry_id));
    past
}

/// One step down the intensity ladder. `None` at the bottom, and for a workout
/// whose category says nothing about intensity.
pub fn ease(category: WorkoutCategory) -> Option<WorkoutCategory> {
    use WorkoutCategory::*;
    match category {
        Anaerobic => Some(Vo2Max),
        Vo2Max => Some(Threshold),
        Threshold => Some(SweetSpot),
        SweetSpot => Some(Tempo),
        Tempo => Some(Endurance),
        Endurance => Some(Recovery),
        Recovery | Custom => None,
    }
}

/// One rung *up* the intensity ladder, for the categories where proposing that
/// is defensible coaching.
///
/// Deliberately not the inverse of [`ease`]. A recovery day is easy on purpose
/// and the plan put it there; a session that is already hard is hard enough.
/// Feeling good is a reason to put some quality into an easy ride, never a
/// reason to sharpen one that was already going to hurt — so the ladder stops
/// at the first hard rung instead of climbing through it.
pub fn push(category: WorkoutCategory) -> Option<WorkoutCategory> {
    use WorkoutCategory::*;
    match category {
        Endurance => Some(Tempo),
        Tempo => Some(SweetSpot),
        SweetSpot => Some(Threshold),
        Recovery | Threshold | Vo2Max | Anaerobic | Custom => None,
    }
}

/// True for the categories that demand a rider be ready for them.
pub fn is_hard(category: WorkoutCategory) -> bool {
    matches!(
        category,
        WorkoutCategory::Threshold | WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic
    )
}

/// The workout in `library` of the given category closest in length to
/// `duration_secs`.
///
/// Length is matched because the rider planned their day around it: a session
/// that eases 60 minutes of threshold into 20 minutes of tempo has solved the
/// intensity problem by inventing a scheduling one.
pub fn pick_replacement(
    library: &[Workout],
    category: WorkoutCategory,
    duration_secs: u32,
) -> Option<&Workout> {
    library
        .iter()
        .filter(|w| w.category == category)
        .min_by_key(|w| w.duration_secs.abs_diff(duration_secs))
}

/// Is resting heart rate up, or sleep poor, enough to ease a session over?
fn wellness_reason(wellness: &[WellnessEntry], today: NaiveDate) -> Option<Reason> {
    let rhr = build_wellness_series(wellness, today, |e| e.resting_hr.map(|v| v as f32));
    let readings: Vec<f32> = rhr.iter().copied().filter(|&v| v > 0.0).collect();

    if readings.len() >= MIN_WELLNESS_READINGS {
        // The baseline excludes today's reading, so a spike is measured against
        // the rider's normal rather than diluted by itself.
        let (latest, earlier) = readings.split_last().expect("length checked above");
        let average = earlier.iter().sum::<f32>() / earlier.len() as f32;
        if average > 0.0 && *latest > average * (1.0 + RESTING_HR_ELEVATION) {
            return Some(Reason::WellnessDip {
                resting_hr_pct: Some((latest - average) / average * 100.0),
            });
        }
    }

    let sleep = build_wellness_series(wellness, today, |e| e.sleep_score.map(|v| v as f32));
    let latest_sleep = sleep.iter().rev().find(|&&v| v > 0.0).copied();
    if latest_sleep.is_some_and(|s| s <= POOR_SLEEP_SCORE) {
        return Some(Reason::WellnessDip {
            resting_hr_pct: None,
        });
    }

    None
}

/// The baseline for one wellness signal, over the window ending the day before
/// `day`.
///
/// Ends the day before on purpose: a baseline that includes today's reading is
/// diluted by the very number being tested against it.
fn wellness_baseline(
    wellness: &[WellnessEntry],
    day: NaiveDate,
    extract: impl Fn(&WellnessEntry) -> Option<f32>,
) -> Option<f32> {
    let prior = day.pred_opt()?;
    let vals: Vec<f32> = build_wellness_series(wellness, prior, extract)
        .into_iter()
        .filter(|&v| v > 0.0)
        .collect();
    (vals.len() >= MIN_WELLNESS_READINGS).then(|| vals.iter().sum::<f32>() / vals.len() as f32)
}

/// Whether every wellness signal recorded for `day` says the rider is recovered,
/// and enough of them exist to mean it.
///
/// Stricter than the inverse of [`wellness_reason`], which fires on any single
/// warning. A warning is allowed to be lonely because the cost of believing one
/// wrongly is a light day. This is not allowed to be lonely, because the cost of
/// believing it wrongly is a hard session on a body that did not want one.
fn is_strong_morning(wellness: &[WellnessEntry], day: NaiveDate) -> bool {
    // The reading must be for this day. Falling back to the most recent one is
    // how a two-day-old HRV comes to be read as this morning's.
    let Some(entry) = wellness.iter().find(|e| e.date == day) else {
        return false;
    };

    let mut signals: Vec<bool> = Vec::new();

    if let (Some(rhr), Some(base)) = (
        entry.resting_hr.map(|v| v as f32),
        wellness_baseline(wellness, day, |e| e.resting_hr.map(|v| v as f32)),
    ) {
        signals.push(rhr < base * (1.0 - RESTING_HR_SUPPRESSION));
    }
    if let (Some(hrv), Some(base)) = (entry.hrv, wellness_baseline(wellness, day, |e| e.hrv)) {
        signals.push(hrv > base * (1.0 + HRV_ELEVATION));
    }
    if let Some(score) = entry.sleep_score {
        signals.push(score as f32 >= GOOD_SLEEP_SCORE);
    }

    signals.len() >= SIGNALS_FOR_STRONG_MORNING && signals.iter().all(|&s| s)
}

/// The one path that proposes more work than the program wrote.
///
/// Every gate here is a way of saying "not on this evidence". They are separate
/// rather than one condition because each answers a different objection: that
/// the signals are stale, that the session was already moved, that the rider is
/// actually tired, that one good night proved nothing, that the week has had its
/// addition already, and that the session was hard to begin with.
fn push_suggestion(
    status: &ProgramStatus,
    decided: &[PlannedSession],
    target: &PlannedSession,
    metrics: &LoadMetrics,
    wellness: &[WellnessEntry],
    library: &[Workout],
    today: NaiveDate,
) -> Vec<Adjustment> {
    // This morning's signals speak for this morning. A session two days out
    // would be ridden on evidence that does not exist yet.
    if target.date != today {
        return Vec::new();
    }

    // A session the rules have already moved is not one to move again. It also
    // keeps this from undoing an ease by the back door.
    if target.adjusted_from.is_some() {
        return Vec::new();
    }

    if !TsbBand::of(metrics.tsb()).is_fresh() {
        return Vec::new();
    }

    let strong = (0..STRONG_MORNINGS_FOR_PUSH as i64).all(|back| {
        today
            .checked_sub_signed(chrono::Duration::days(back))
            .is_some_and(|day| is_strong_morning(wellness, day))
    });
    if !strong {
        return Vec::new();
    }

    // One addition per rolling week. Two in quick succession is a ramp the
    // program never planned and nothing here is tracking the cost of.
    let adjusted_recently = decided
        .iter()
        .chain(status.upcoming.iter())
        .any(|s| s.adjusted_from.is_some() && (today - s.date).num_days().abs() < 7);
    if adjusted_recently {
        return Vec::new();
    }

    let Some(harder) = push(target.category) else {
        return Vec::new();
    };
    let Some(replacement) = pick_replacement(library, harder, target.duration_secs) else {
        return Vec::new();
    };
    if replacement.id == target.workout_id {
        return Vec::new();
    }

    vec![Adjustment {
        entry_id: target.entry_id,
        date: target.date,
        from_workout_id: target.workout_id,
        from_name: target.workout_name.clone(),
        to_workout_id: replacement.id,
        to_name: replacement.name.clone(),
        reason: Reason::Primed {
            mornings: STRONG_MORNINGS_FOR_PUSH,
        },
    }]
}

/// What, if anything, should change about the sessions still to come.
///
/// Returns at most one adjustment — the next session only. Form is recomputed
/// every day, so easing the whole week in advance would be deciding on
/// Wednesday's behalf using Monday's evidence.
///
/// `verdict` is what the morning brief made of today. It is a reason to ease,
/// never a way to pick the session: this module remains the only thing that
/// produces an [`Adjustment`], so a rider is never shown two different answers
/// to the same question.
pub fn suggest(
    status: &ProgramStatus,
    decided: &[PlannedSession],
    metrics: &LoadMetrics,
    wellness: &[WellnessEntry],
    library: &[Workout],
    today: NaiveDate,
    verdict: CoachVerdict,
) -> Vec<Adjustment> {
    // A recovery week is already the easy week. There is nothing to take out of
    // it, and nothing may be put in — including by the brief, which sees one
    // morning and not the shape of the block around it.
    if status.phase == Phase::Recovery {
        return Vec::new();
    }

    let Some(target) = status.upcoming.first() else {
        return Vec::new();
    };

    // The brief speaks for today and only for today. On a rest day the next
    // session is Wednesday's, and this morning's wellness says nothing about
    // it — the same reason this function only ever adjusts one session.
    let verdict = if target.date == today {
        verdict
    } else {
        CoachVerdict::Proceed
    };

    let tsb = metrics.tsb();
    let mut reasons: Vec<Reason> = Vec::new();
    if TsbBand::of(tsb).is_fatigued() {
        reasons.push(Reason::Fatigued { tsb });
    }
    if let Some(reason) = wellness_reason(wellness, today) {
        reasons.push(reason);
    }
    let run = status.missed_run(decided);
    if run >= MISSED_RUN_FOR_EASING {
        reasons.push(Reason::MissedRun { count: run });
    }
    if verdict != CoachVerdict::Proceed {
        reasons.push(Reason::CoachAdvised);
    }

    let Some(reason) = reasons.into_iter().max_by_key(|r| r.severity()) else {
        // Nothing wants this session easier. The only branch that adds work,
        // and it is reachable only from here — so an easing reason of any kind,
        // however weak, always outranks wanting to push.
        return push_suggestion(status, decided, target, metrics, wellness, library, today);
    };

    // Missing sessions is a reason to be careful with hard work, not a reason
    // to downgrade an endurance ride the rider is perfectly able to do.
    if matches!(reason, Reason::MissedRun { .. }) && !is_hard(target.category) {
        return Vec::new();
    }

    // A brief asking for rest is not asking for a slightly easier session, so
    // one rung down the ladder would be answering a question it did not ask.
    // Recovery is as far as this module goes: dropping the day outright is the
    // rider's call, never the plan's — see the module note.
    let eased = if verdict == CoachVerdict::Rest {
        (target.category != WorkoutCategory::Recovery).then_some(WorkoutCategory::Recovery)
    } else {
        ease(target.category)
    };
    let Some(eased) = eased else {
        return Vec::new();
    };
    let Some(replacement) = pick_replacement(library, eased, target.duration_secs) else {
        return Vec::new();
    };
    // The library can hold only one workout of a category, and it may be the
    // one already scheduled.
    if replacement.id == target.workout_id {
        return Vec::new();
    }

    vec![Adjustment {
        entry_id: target.entry_id,
        date: target.date,
        from_workout_id: target.workout_id,
        from_name: target.workout_name.clone(),
        to_workout_id: replacement.id,
        to_name: replacement.name.clone(),
        reason,
    }]
}

/// The program's current state and what it proposes changing, in one call.
///
/// Two surfaces ask this question now — the Coaching page's card and the
/// calendar — and they must never answer it differently. Bundling the three
/// steps here means neither can accidentally call [`suggest`] with a
/// differently-derived [`ProgramStatus`].
#[allow(clippy::too_many_arguments)] // one call, threading the plan's whole world
pub fn plan_view(
    program: &Program,
    sessions: &[PlannedSession],
    trained: &std::collections::HashSet<NaiveDate>,
    metrics: &LoadMetrics,
    wellness: &[WellnessEntry],
    library: &[Workout],
    today: NaiveDate,
    verdict: CoachVerdict,
) -> (ProgramStatus, Vec<Adjustment>) {
    // Annotate first, so `status` and `decided` cannot disagree about which days
    // were trained.
    let sessions = mark_trained(sessions, trained);
    let sessions = sessions.as_slice();
    let state = status(program, sessions, today);
    let past = decided(sessions, today);
    let adjustments = suggest(&state, &past, metrics, wellness, library, today, verdict);
    (state, adjustments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;
    use std::collections::HashSet;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    /// Monday 3 August 2026.
    fn start() -> NaiveDate {
        date(2026, 8, 3)
    }

    fn program(weeks: u32) -> Program {
        Program {
            id: 1,
            start_monday: start(),
            num_weeks: weeks,
            training_days: "monday,wednesday,friday".into(),
        }
    }

    /// [`suggest`] with no brief for the day — the local rules on their own.
    ///
    /// Most of these tests predate the brief and are the regression net proving
    /// it changed nothing about them, so they say so by construction rather
    /// than by threading a `Proceed` through every call.
    fn suggest_local(
        status: &ProgramStatus,
        decided: &[PlannedSession],
        metrics: &LoadMetrics,
        wellness: &[WellnessEntry],
        library: &[Workout],
        today: NaiveDate,
    ) -> Vec<Adjustment> {
        suggest(
            status,
            decided,
            metrics,
            wellness,
            library,
            today,
            CoachVerdict::Proceed,
        )
    }

    #[test]
    fn plan_view_should_agree_with_calling_the_three_steps_separately() {
        // The calendar and the Coaching card both go through `plan_view`; this
        // is what stops it drifting from the pieces the rules are tested on.
        let today = date(2026, 8, 12);
        let sessions = vec![
            session(1, date(2026, 8, 10), WorkoutCategory::Endurance, false),
            session(2, today, WorkoutCategory::Threshold, false),
            session(3, date(2026, 8, 14), WorkoutCategory::Vo2Max, false),
        ];
        let m = metrics(-22.0);
        let w = wellness(&[], today);
        let lib = library();
        let prog = program(8);

        let (state, adjustments) = plan_view(
            &prog,
            &sessions,
            &Default::default(),
            &m,
            &w,
            &lib,
            today,
            CoachVerdict::Proceed,
        );

        let expected_state = status(&prog, &sessions, today);
        let expected = suggest(
            &expected_state,
            &decided(&sessions, today),
            &m,
            &w,
            &lib,
            today,
            CoachVerdict::Proceed,
        );

        assert_eq!(state, expected_state);
        assert_eq!(adjustments, expected);
        // And it is actually exercising the interesting path, not two empties.
        assert_eq!(adjustments.len(), 1);
    }

    fn session(id: i64, d: NaiveDate, cat: WorkoutCategory, completed: bool) -> PlannedSession {
        PlannedSession {
            trained: false,
            entry_id: id,
            date: d,
            workout_id: id * 10,
            workout_name: format!("{} Session", cat.label()),
            category: cat,
            tss: 60.0,
            duration_secs: 3600,
            completed,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    fn workout(id: i64, cat: WorkoutCategory, duration_secs: u32) -> Workout {
        Workout {
            id,
            name: format!("{} {}", cat.label(), duration_secs / 60),
            description: String::new(),
            duration_secs,
            tss: 50.0,
            category: cat,
            segments: vec![Segment::steady(duration_secs, 60.0, "Ride")],
        }
    }

    /// A library with one workout per category, all the same length.
    fn library() -> Vec<Workout> {
        use WorkoutCategory::*;
        [
            Recovery, Endurance, Tempo, SweetSpot, Threshold, Vo2Max, Anaerobic,
        ]
        .iter()
        .enumerate()
        .map(|(i, &c)| workout(i as i64 + 1, c, 3600))
        .collect()
    }

    fn metrics(tsb: f64) -> LoadMetrics {
        // TSB is CTL - ATL, so fix CTL and derive the fatigue that yields it.
        LoadMetrics {
            ctl: 50.0,
            atl: 50.0 - tsb,
            ctl_4wk_ago: 50.0,
        }
    }

    fn wellness(days: &[(u32, u32)], from: NaiveDate) -> Vec<WellnessEntry> {
        // (days_ago, resting_hr)
        days.iter()
            .map(|&(ago, rhr)| WellnessEntry {
                date: from - chrono::Duration::days(ago as i64),
                hrv: None,
                resting_hr: Some(rhr),
                sleep_secs: None,
                sleep_score: None,
                steps: None,
                calories: None,
            })
            .collect()
    }

    // ── Weeks and phases ──────────────────────────────────────────────────────

    #[test]
    fn should_call_the_starting_monday_week_one() {
        assert_eq!(week_of(&program(12), start()), 1);
    }

    #[test]
    fn should_keep_the_whole_first_week_in_week_one() {
        // Sunday closes the week it belongs to.
        assert_eq!(week_of(&program(12), date(2026, 8, 9)), 1);
    }

    #[test]
    fn should_advance_a_week_on_the_next_monday() {
        assert_eq!(week_of(&program(12), date(2026, 8, 10)), 2);
    }

    #[test]
    fn should_read_a_date_before_the_start_as_week_one() {
        assert_eq!(week_of(&program(12), date(2026, 7, 30)), 1);
    }

    #[test]
    fn should_clamp_a_date_past_the_end_to_the_final_week() {
        // A plan the rider has run past still describes itself.
        assert_eq!(week_of(&program(4), date(2026, 12, 25)), 4);
    }

    #[test]
    fn should_cross_a_month_boundary_correctly() {
        assert_eq!(week_of(&program(12), date(2026, 9, 1)), 5);
    }

    #[test]
    fn should_make_every_fourth_week_a_recovery_week() {
        assert_eq!(phase_of(4), Phase::Recovery);
        assert_eq!(phase_of(8), Phase::Recovery);
        for week in [1, 2, 3, 5, 6, 7, 9] {
            assert_eq!(phase_of(week), Phase::Build, "week {week}");
        }
    }

    #[test]
    fn should_never_reach_a_recovery_week_in_a_short_program() {
        for week in 1..=3 {
            assert_eq!(phase_of(week), Phase::Build);
        }
    }

    // ── Status ────────────────────────────────────────────────────────────────

    #[test]
    fn should_not_count_a_day_that_was_ridden_as_missed() {
        // The bug this whole release exists for: this rider trains mostly
        // outdoors, and every outdoor day read as a skipped session.
        let sessions = vec![
            session(1, date(2026, 8, 4), WorkoutCategory::Tempo, false),
            session(2, date(2026, 8, 5), WorkoutCategory::Recovery, false),
        ];
        let marked = mark_trained(&sessions, &HashSet::from([date(2026, 8, 4)]));

        let s = status(&program(12), &marked, date(2026, 8, 10));
        assert_eq!(
            s.missed.iter().map(|m| m.date).collect::<Vec<_>>(),
            vec![date(2026, 8, 5)],
            "the 4th was ridden through; only the 5th was skipped"
        );
    }

    #[test]
    fn should_keep_a_ridden_day_out_of_the_completed_tally() {
        // Riding *something* is not doing the planned session. The two answers
        // stay separate; only "missed" merges them.
        let sessions = vec![session(1, date(2026, 8, 4), WorkoutCategory::Tempo, false)];
        let marked = mark_trained(&sessions, &HashSet::from([date(2026, 8, 4)]));

        let s = status(&program(12), &marked, date(2026, 8, 10));
        assert_eq!(s.completed, 0, "the planned session was still not done");
        assert!(s.missed.is_empty(), "but the day was not skipped either");
    }

    #[test]
    fn should_break_the_missed_run_on_a_day_that_was_ridden() {
        // `missed_run` is what eases every hard session at two in a row, so a
        // ridden day has to break it or the plan keeps backing off for a reason
        // that is not true.
        let sessions = vec![
            session(1, date(2026, 8, 4), WorkoutCategory::Threshold, false),
            session(2, date(2026, 8, 6), WorkoutCategory::Threshold, false),
            session(3, date(2026, 8, 8), WorkoutCategory::Threshold, false),
        ];
        let marked = mark_trained(&sessions, &HashSet::from([date(2026, 8, 8)]));
        let past = decided(&marked, date(2026, 8, 10));
        let s = status(&program(12), &marked, date(2026, 8, 10));

        assert_eq!(
            s.missed_run(&past),
            0,
            "the most recent planned day was ridden through"
        );
    }

    #[test]
    fn should_still_count_a_run_of_days_with_no_riding_at_all() {
        let sessions = vec![
            session(1, date(2026, 8, 4), WorkoutCategory::Threshold, false),
            session(2, date(2026, 8, 6), WorkoutCategory::Threshold, false),
        ];
        let marked = mark_trained(&sessions, &HashSet::from([date(2026, 8, 1)]));
        let past = decided(&marked, date(2026, 8, 10));
        let s = status(&program(12), &marked, date(2026, 8, 10));

        assert_eq!(s.missed_run(&past), 2, "neither day was ridden");
    }

    #[test]
    fn should_split_sessions_into_missed_and_upcoming() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, true),
            session(2, date(2026, 8, 5), Threshold, false), // missed
            session(3, date(2026, 8, 7), Tempo, true),
            session(4, date(2026, 8, 10), Vo2Max, false), // upcoming
        ];
        let s = status(&program(12), &sessions, date(2026, 8, 8));

        assert_eq!(s.planned, 4);
        assert_eq!(s.completed, 2);
        assert_eq!(s.missed.len(), 1);
        assert_eq!(s.missed[0].entry_id, 2);
        assert_eq!(s.upcoming.len(), 1);
        assert_eq!(s.upcoming[0].entry_id, 4);
    }

    #[test]
    fn should_list_only_the_last_fortnights_missed_sessions_as_recent() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 24);
        let sessions = vec![
            session(1, date(2026, 6, 16), Endurance, false), // long gone
            session(2, date(2026, 8, 6), Tempo, false),      // 18 days back
            session(3, date(2026, 8, 12), Threshold, false), // 12 days back
            session(4, date(2026, 8, 19), Vo2Max, false),    // 5 days back
        ];
        let s = status(&program(15), &sessions, today);

        assert_eq!(s.missed.len(), 4, "the full history is untouched");
        let recent: Vec<i64> = s.missed_recent.iter().map(|s| s.entry_id).collect();
        assert_eq!(recent, vec![3, 4]);
    }

    #[test]
    fn should_keep_the_full_missed_history_alongside_the_recent_one() {
        // The two fields answer different questions, and nothing may quietly
        // lose the long view: a program adopted from a bare calendar infers its
        // training days from every day it ever used.
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 6, 16), Endurance, false),
            session(2, date(2026, 8, 19), Vo2Max, false),
        ];
        let s = status(&program(15), &sessions, date(2026, 8, 24));

        assert_eq!(s.missed.len(), 2);
        assert_eq!(s.missed_recent.len(), 1);
        assert!(s.missed.len() > s.missed_recent.len());
    }

    #[test]
    fn should_count_a_session_missed_exactly_fourteen_days_ago_as_recent() {
        let sessions = vec![session(
            1,
            date(2026, 8, 10),
            WorkoutCategory::Threshold,
            false,
        )];
        let s = status(&program(15), &sessions, date(2026, 8, 24));

        assert_eq!(s.missed_recent.len(), 1, "the boundary is inclusive");
    }

    #[test]
    fn should_drop_a_session_missed_fifteen_days_ago_from_recent() {
        let sessions = vec![session(
            1,
            date(2026, 8, 9),
            WorkoutCategory::Threshold,
            false,
        )];
        let s = status(&program(15), &sessions, date(2026, 8, 24));

        assert!(s.missed_recent.is_empty());
        assert_eq!(s.missed.len(), 1, "still missed, just no longer recent");
    }

    #[test]
    fn should_treat_todays_unridden_session_as_upcoming_not_missed() {
        // The day is not over — nothing has been missed yet.
        let sessions = vec![session(
            1,
            date(2026, 8, 5),
            WorkoutCategory::Threshold,
            false,
        )];
        let s = status(&program(12), &sessions, date(2026, 8, 5));
        assert!(s.missed.is_empty());
        assert_eq!(s.upcoming.len(), 1);
    }

    #[test]
    fn should_order_upcoming_sessions_soonest_first() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(2, date(2026, 8, 14), Vo2Max, false),
            session(1, date(2026, 8, 10), Tempo, false),
        ];
        let s = status(&program(12), &sessions, date(2026, 8, 8));
        assert_eq!(s.upcoming[0].entry_id, 1);
    }

    #[test]
    fn should_count_a_run_of_missed_sessions_from_the_most_recent() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, false),
            session(2, date(2026, 8, 5), Threshold, true),
            session(3, date(2026, 8, 7), Tempo, false),
            session(4, date(2026, 8, 9), Vo2Max, false),
        ];
        let past = decided(&sessions, date(2026, 8, 10));
        let s = status(&program(12), &sessions, date(2026, 8, 10));
        assert_eq!(s.missed_run(&past), 2, "the two most recent, not all three");
    }

    #[test]
    fn should_score_a_rider_who_is_back_on_it_as_no_run() {
        use WorkoutCategory::*;
        let sessions = vec![
            session(1, date(2026, 8, 3), Endurance, false),
            session(2, date(2026, 8, 5), Threshold, false),
            session(3, date(2026, 8, 7), Tempo, true),
        ];
        let past = decided(&sessions, date(2026, 8, 8));
        let s = status(&program(12), &sessions, date(2026, 8, 8));
        assert_eq!(s.missed_run(&past), 0);
    }

    // ── The ladder ────────────────────────────────────────────────────────────

    #[test]
    fn should_step_down_one_rung_at_a_time() {
        use WorkoutCategory::*;
        assert_eq!(ease(Anaerobic), Some(Vo2Max));
        assert_eq!(ease(Vo2Max), Some(Threshold));
        assert_eq!(ease(Threshold), Some(SweetSpot));
        assert_eq!(ease(SweetSpot), Some(Tempo));
        assert_eq!(ease(Tempo), Some(Endurance));
        assert_eq!(ease(Endurance), Some(Recovery));
    }

    #[test]
    fn should_stop_at_the_bottom_of_the_ladder() {
        assert_eq!(ease(WorkoutCategory::Recovery), None);
    }

    #[test]
    fn should_not_guess_at_a_custom_workouts_intensity() {
        // A custom workout's category says nothing about how hard it is.
        assert_eq!(ease(WorkoutCategory::Custom), None);
    }

    #[test]
    fn should_terminate_when_walked_all_the_way_down() {
        let mut category = WorkoutCategory::Anaerobic;
        let mut steps = 0;
        while let Some(next) = ease(category) {
            category = next;
            steps += 1;
            assert!(steps < 10, "the ladder must not cycle");
        }
        assert_eq!(category, WorkoutCategory::Recovery);
    }

    #[test]
    fn should_pick_the_replacement_closest_in_length() {
        use WorkoutCategory::*;
        let library = vec![
            workout(1, Endurance, 1800),
            workout(2, Endurance, 3600),
            workout(3, Endurance, 7200),
        ];
        let picked = pick_replacement(&library, Endurance, 3300).expect("an endurance workout");
        assert_eq!(picked.duration_secs, 3600);
    }

    #[test]
    fn should_find_nothing_when_the_library_has_no_such_workout() {
        let library = vec![workout(1, WorkoutCategory::Endurance, 3600)];
        assert!(pick_replacement(&library, WorkoutCategory::Tempo, 3600).is_none());
    }

    // ── Adding work on a strong morning ───────────────────────────────────────

    /// A status whose next session is *today's* — the only day a push may touch.
    fn today_status(category: WorkoutCategory, today: NaiveDate) -> ProgramStatus {
        let sessions = vec![session(1, today, category, false)];
        status(&program(12), &sessions, today)
    }

    /// `strong` mornings ending today, on top of a settled ordinary baseline.
    fn strong_wellness(today: NaiveDate, strong: i64) -> Vec<WellnessEntry> {
        let ordinary = |ago: i64| WellnessEntry {
            date: today - chrono::Duration::days(ago),
            hrv: Some(50.0),
            resting_hr: Some(50),
            sleep_secs: None,
            sleep_score: Some(70),
            steps: None,
            calories: None,
        };
        let recovered = |ago: i64| WellnessEntry {
            hrv: Some(60.0),
            resting_hr: Some(45),
            sleep_score: Some(88),
            ..ordinary(ago)
        };
        (strong..strong + 6)
            .map(ordinary)
            .chain((0..strong).map(recovered))
            .collect()
    }

    #[test]
    fn should_step_up_todays_session_when_every_signal_is_strong_two_mornings_running() {
        let today = date(2026, 8, 5);
        let s = today_status(WorkoutCategory::Endurance, today);
        let out = suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today,
        );

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, Reason::Primed { mornings: 2 }));
        assert_eq!(out[0].to_name, "Tempo 60");
    }

    #[test]
    fn should_not_step_up_when_only_one_morning_is_strong() {
        // One good night is a late dinner, not a training decision.
        let today = date(2026, 8, 5);
        let s = today_status(WorkoutCategory::Endurance, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 1),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_when_form_is_not_fresh() {
        // Every wellness signal can agree and still not outvote a TSB of zero.
        let today = date(2026, 8, 5);
        let s = today_status(WorkoutCategory::Endurance, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(0.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_a_session_that_is_already_hard() {
        let today = date(2026, 8, 5);
        let s = today_status(WorkoutCategory::Threshold, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_a_recovery_day_the_plan_put_there_on_purpose() {
        let today = date(2026, 8, 5);
        let s = today_status(WorkoutCategory::Recovery, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_a_session_the_rules_have_already_moved() {
        let today = date(2026, 8, 5);
        let mut sessions = vec![session(1, today, WorkoutCategory::Endurance, false)];
        sessions[0].adjusted_from = Some("Tempo 60".into());
        let s = status(&program(12), &sessions, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_a_session_that_is_not_todays() {
        // ready_status puts the session tomorrow. This morning's signals say
        // nothing about a ride that happens after another night's sleep.
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Endurance, today);
        assert!(suggest_local(
            &s,
            &[],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_not_step_up_twice_in_one_rolling_week() {
        let today = date(2026, 8, 5);
        let mut earlier = session(
            2,
            today - chrono::Duration::days(3),
            WorkoutCategory::Tempo,
            true,
        );
        earlier.adjusted_from = Some("Endurance 60".into());
        let sessions = vec![
            earlier.clone(),
            session(1, today, WorkoutCategory::Endurance, false),
        ];
        let s = status(&program(12), &sessions, today);
        assert!(suggest_local(
            &s,
            &[earlier],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today
        )
        .is_empty());
    }

    #[test]
    fn should_ease_rather_than_add_when_any_warning_applies_at_all() {
        // Strong wellness and fresh form, but sessions were missed back to back.
        // The weakest easing reason still outranks wanting to push.
        let today = date(2026, 8, 5);
        let missed_a = session(
            8,
            today - chrono::Duration::days(4),
            WorkoutCategory::Threshold,
            false,
        );
        let missed_b = session(
            9,
            today - chrono::Duration::days(2),
            WorkoutCategory::Threshold,
            false,
        );
        let sessions = vec![
            missed_a.clone(),
            missed_b.clone(),
            session(1, today, WorkoutCategory::Endurance, false),
        ];
        let s = status(&program(12), &sessions, today);
        let out = suggest_local(
            &s,
            &[missed_a, missed_b],
            &metrics(12.0),
            &strong_wellness(today, 2),
            &library(),
            today,
        );
        assert!(out
            .iter()
            .all(|a| !matches!(a.reason, Reason::Primed { .. })));
    }

    #[test]
    fn should_climb_one_rung_and_stop_at_the_first_hard_one() {
        use WorkoutCategory::*;
        assert_eq!(push(Endurance), Some(Tempo));
        assert_eq!(push(Tempo), Some(SweetSpot));
        assert_eq!(push(SweetSpot), Some(Threshold));
        // Already hard, or deliberately easy: nothing above these.
        assert_eq!(push(Threshold), None);
        assert_eq!(push(Vo2Max), None);
        assert_eq!(push(Anaerobic), None);
        assert_eq!(push(Recovery), None);
        assert_eq!(push(Custom), None);
    }

    #[test]
    fn should_not_call_a_morning_strong_when_one_signal_disagrees() {
        let today = date(2026, 8, 5);
        let mut w = strong_wellness(today, 2);
        // Slept badly last night; everything else still looks recovered.
        w.last_mut().expect("today's entry").sleep_score = Some(40);
        let s = today_status(WorkoutCategory::Endurance, today);
        assert!(suggest_local(&s, &[], &metrics(12.0), &w, &library(), today).is_empty());
    }

    #[test]
    fn should_not_call_a_morning_strong_when_there_is_no_reading_for_it() {
        // The staleness trap: yesterday was strong and today has no entry at
        // all. Reading yesterday's numbers as this morning's is the mistake.
        let today = date(2026, 8, 5);
        let w: Vec<WellnessEntry> = strong_wellness(today, 2)
            .into_iter()
            .filter(|e| e.date != today)
            .collect();
        let s = today_status(WorkoutCategory::Endurance, today);
        assert!(suggest_local(&s, &[], &metrics(12.0), &w, &library(), today).is_empty());
    }

    // ── The rules ─────────────────────────────────────────────────────────────

    /// A program with one hard session coming up and nothing else going on.
    fn ready_status(category: WorkoutCategory, today: NaiveDate) -> ProgramStatus {
        let sessions = vec![session(
            1,
            today + chrono::Duration::days(1),
            category,
            false,
        )];
        status(&program(12), &sessions, today)
    }

    #[test]
    fn should_ease_the_next_session_when_form_is_dug_in() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        let out = suggest_local(&s, &[], &metrics(-35.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entry_id, 1);
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
        assert_eq!(out[0].to_name, "Sweet Spot 60");
    }

    #[test]
    fn should_leave_a_rested_rider_alone() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        assert!(suggest_local(&s, &[], &metrics(0.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_never_touch_a_recovery_week() {
        // Week 4 is already the easy week — nothing comes out of it.
        let today = date(2026, 8, 26); // week 4 of a program starting 3 Aug
        let s = ready_status(WorkoutCategory::Threshold, today);
        assert_eq!(s.phase, Phase::Recovery);
        assert!(suggest_local(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_ease_after_two_missed_sessions_in_a_row() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        let out = suggest_local(&s, &past, &metrics(5.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, Reason::MissedRun { count: 2 }));
    }

    #[test]
    fn should_stop_easing_once_the_missed_sessions_are_marked_done() {
        // The claim 0.6.0 rests on. Windowing `missed` only makes the card
        // honest; what actually stops a false easing is the rider closing those
        // sessions by hand, which empties the run `suggest` reads.
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let ridden_outdoors = vec![
            session(1, date(2026, 8, 5), Tempo, true),
            session(2, date(2026, 8, 7), Tempo, true),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &ridden_outdoors, today);
        let past = decided(&ridden_outdoors, today);
        let out = suggest_local(&s, &past, &metrics(5.0), &[], &library(), today);

        assert!(
            out.is_empty(),
            "nothing was missed, so nothing should be eased"
        );
    }

    #[test]
    fn should_not_ease_after_a_single_missed_session() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, true),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        assert!(suggest_local(&s, &past, &metrics(5.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_not_downgrade_an_easy_ride_over_missed_sessions() {
        // Missing sessions is a reason to be careful with hard work, not a
        // reason to take an endurance ride away.
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Endurance, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        assert!(suggest_local(&s, &past, &metrics(5.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_ease_when_resting_heart_rate_is_up_on_its_baseline() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // Steady at 50, then 58 today — 16 % up.
        let w = wellness(&[(4, 50), (3, 50), (2, 50), (1, 50), (0, 58)], today);
        let out = suggest_local(&s, &[], &metrics(0.0), &w, &library(), today);

        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].reason,
            Reason::WellnessDip {
                resting_hr_pct: Some(_)
            }
        ));
    }

    #[test]
    fn should_ignore_normal_day_to_day_variation_in_resting_heart_rate() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // 51 against a baseline of 50 — 2 %, inside normal variation.
        let w = wellness(&[(4, 50), (3, 50), (2, 50), (1, 50), (0, 51)], today);
        assert!(suggest_local(&s, &[], &metrics(0.0), &w, &library(), today).is_empty());
    }

    #[test]
    fn should_want_a_baseline_before_calling_a_reading_elevated() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // Two readings is not a baseline, however alarming the second looks.
        let w = wellness(&[(1, 50), (0, 70)], today);
        assert!(suggest_local(&s, &[], &metrics(0.0), &w, &library(), today).is_empty());
    }

    #[test]
    fn should_let_fatigue_outrank_a_missed_run() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 10);
        let sessions = vec![
            session(1, date(2026, 8, 5), Tempo, false),
            session(2, date(2026, 8, 7), Tempo, false),
            session(3, date(2026, 8, 12), Vo2Max, false),
        ];
        let s = status(&program(12), &sessions, today);
        let past = decided(&sessions, today);
        let out = suggest_local(&s, &past, &metrics(-35.0), &[], &library(), today);

        assert_eq!(out.len(), 1, "one adjustment, not one per reason");
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
    }

    #[test]
    fn should_only_ever_adjust_the_next_session() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 5);
        let sessions = vec![
            session(1, date(2026, 8, 7), Threshold, false),
            session(2, date(2026, 8, 10), Vo2Max, false),
            session(3, date(2026, 8, 12), Anaerobic, false),
        ];
        let s = status(&program(12), &sessions, today);
        let out = suggest_local(&s, &[], &metrics(-40.0), &[], &library(), today);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entry_id, 1, "the soonest one");
    }

    #[test]
    fn should_propose_nothing_when_the_plan_is_finished() {
        let today = date(2026, 8, 5);
        let s = status(&program(12), &[], today);
        assert!(suggest_local(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_propose_nothing_when_the_library_cannot_supply_a_replacement() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        // No sweet spot workout to step down to.
        let sparse = vec![workout(1, WorkoutCategory::Vo2Max, 3600)];
        assert!(suggest_local(&s, &[], &metrics(-40.0), &[], &sparse, today).is_empty());
    }

    #[test]
    fn should_propose_nothing_when_the_session_is_already_the_easiest_there_is() {
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Recovery, today);
        assert!(suggest_local(&s, &[], &metrics(-40.0), &[], &library(), today).is_empty());
    }

    #[test]
    fn should_never_propose_swapping_a_workout_for_itself() {
        use WorkoutCategory::*;
        let today = date(2026, 8, 5);
        // The scheduled session *is* the only endurance workout in the library,
        // and easing tempo lands on endurance.
        let mut sessions = vec![session(1, date(2026, 8, 7), Tempo, false)];
        sessions[0].workout_id = 2;
        sessions[0].duration_secs = 3600;
        let s = status(&program(12), &sessions, today);
        let library = vec![workout(2, Endurance, 3600)];
        assert!(suggest_local(&s, &[], &metrics(-40.0), &[], &library, today).is_empty());
    }

    // ── The morning brief's verdict ───────────────────────────────────────────

    /// A program whose next session is *today*, which is the only day the
    /// brief's verdict speaks for.
    fn due_today(category: WorkoutCategory, today: NaiveDate) -> ProgramStatus {
        status(&program(12), &[session(1, today, category, false)], today)
    }

    #[test]
    fn should_ease_the_next_session_when_the_coach_advises_it() {
        // Nothing measured is wrong — a rested rider, no wellness dip, nothing
        // missed. The brief is the only thing that noticed.
        let today = date(2026, 8, 5);
        let s = due_today(WorkoutCategory::Threshold, today);
        let out = suggest(
            &s,
            &[],
            &metrics(0.0),
            &[],
            &library(),
            today,
            CoachVerdict::Ease,
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reason, Reason::CoachAdvised);
        assert_eq!(out[0].to_name, "Sweet Spot 60", "one rung down");
    }

    #[test]
    fn should_prefer_a_measured_reason_over_the_coachs_when_both_apply() {
        // "Your form is -35" is worth more to the rider than "your coach said
        // so", so the measurement supplies the sentence.
        let today = date(2026, 8, 5);
        let s = due_today(WorkoutCategory::Threshold, today);
        let out = suggest(
            &s,
            &[],
            &metrics(-35.0),
            &[],
            &library(),
            today,
            CoachVerdict::Ease,
        );

        assert_eq!(out.len(), 1, "one adjustment, not one per reason");
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
    }

    #[test]
    fn should_drop_a_rest_verdict_to_recovery_rather_than_one_rung() {
        // Rest is not a request for a slightly easier session. Easing threshold
        // one rung would land on sweet spot, which is not what was asked.
        let today = date(2026, 8, 5);
        let s = due_today(WorkoutCategory::Threshold, today);
        let out = suggest(
            &s,
            &[],
            &metrics(0.0),
            &[],
            &library(),
            today,
            CoachVerdict::Rest,
        );

        assert_eq!(out.len(), 1);
        // Recovery, and still the hour the rider set aside — easing intensity
        // must not invent a scheduling problem.
        assert_eq!(out[0].to_name, "Recovery 60");
    }

    #[test]
    fn should_change_nothing_when_rest_is_advised_and_the_session_is_already_recovery() {
        let today = date(2026, 8, 5);
        let s = due_today(WorkoutCategory::Recovery, today);
        assert!(suggest(
            &s,
            &[],
            &metrics(0.0),
            &[],
            &library(),
            today,
            CoachVerdict::Rest,
        )
        .is_empty());
    }

    #[test]
    fn should_ignore_the_verdict_when_the_next_session_is_not_today() {
        // Mon/Wed/Fri, and today is Tuesday: the next session is tomorrow's.
        // This morning's readiness says nothing about how the rider will wake
        // up, and the local rules found nothing on their own.
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        assert!(suggest(
            &s,
            &[],
            &metrics(0.0),
            &[],
            &library(),
            today,
            CoachVerdict::Rest,
        )
        .is_empty());
    }

    #[test]
    fn should_still_apply_the_local_rules_when_the_session_is_not_today() {
        // Gating the verdict must not gate the measurements — those read
        // rolling averages and apply whenever the session falls.
        let today = date(2026, 8, 5);
        let s = ready_status(WorkoutCategory::Threshold, today);
        let out = suggest(
            &s,
            &[],
            &metrics(-35.0),
            &[],
            &library(),
            today,
            CoachVerdict::Rest,
        );

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, Reason::Fatigued { .. }));
        assert_eq!(
            out[0].to_name, "Sweet Spot 60",
            "eased, not dropped to rest"
        );
    }

    #[test]
    fn should_change_nothing_in_a_recovery_week_even_when_the_coach_advises_rest() {
        // Week 4 is the recovery week. It is already the easy week, and the
        // brief sees one morning rather than the shape of the block.
        let today = start() + chrono::Duration::weeks(3);
        let s = due_today(WorkoutCategory::Endurance, today);
        assert_eq!(s.phase, Phase::Recovery, "the fixture is a recovery week");
        assert!(suggest(
            &s,
            &[],
            &metrics(0.0),
            &[],
            &library(),
            today,
            CoachVerdict::Rest,
        )
        .is_empty());
    }

    #[test]
    fn should_default_to_proceeding() {
        // A rider with no brief — no key, or the request failed — must get
        // exactly the behaviour they had before the brief existed.
        assert_eq!(CoachVerdict::default(), CoachVerdict::Proceed);
    }
}
