//! The next block, built locally when a program runs out.
//!
//! A finished plan used to leave the rider one offer: pay for an AI rebuild.
//! This is the other one — four weeks assembled from their own library by the
//! same rules that already ease and harden sessions, costing nothing and
//! sending nothing anywhere.
//!
//! It is deliberately dumber than the coach. It does not read goals, it does
//! not reason about a season, and it will not invent a workout. What it
//! guarantees is the thing a rider actually needs from a block that follows
//! another: that it progresses, that it recovers, and that it puts real work in
//! — see [`hard_target`] for why that last one is not merely good coaching but
//! a precondition for the app ever noticing their FTP has moved.
//!
//! Everything here is pure. Dates in, dates out; the caller writes them.

use std::collections::HashSet;

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use crate::data::workout::{Workout, WorkoutCategory};
use crate::training::ftp_detect::{hard_evidence_seconds, MIN_HARD_SECONDS};
use crate::training::program::{
    last_day, phase_of, pick_replacement, Phase, PlannedSession, Program, BLOCK_WEEKS,
};

/// How the weekly volume moves through a block: three building, then the deload.
///
/// Indexed by the week's position in the block, so the shape is stated once
/// rather than reconstructed from a week number at each use.
const RAMP: [f32; BLOCK_WEEKS as usize] = [1.00, 1.06, 1.12, 0.70];

/// Session length assumed when the finished block offers nothing to measure.
const FALLBACK_SESSION_SECS: u32 = 3600;

/// Days off in the week before a block for it to count as rest already taken.
///
/// Five of seven, so a long weekend does not excuse the recovery week but a
/// working trip away does.
const REST_DAYS_FOR_A_TAKEN_BREAK: usize = 5;

/// Weeks a start date may be pushed forward before giving up.
const MAX_START_SEARCH_WEEKS: i64 = 8;

/// Why a block could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RolloverError {
    #[error("No training days were chosen")]
    NoTrainingDays,
    #[error("Your workout library has nothing to build a block from")]
    EmptyLibrary,
    #[error("Every week of the next block falls in your planned time off")]
    AllTimeOff,
}

/// A block, ready to be written to the calendar.
#[derive(Debug, Clone, PartialEq)]
pub struct NextBlock {
    pub start_monday: NaiveDate,
    pub weeks: u32,
    /// `(workout_id, date)`, oldest first — the shape
    /// [`crate::ai::context::drop_time_off_days`] takes.
    pub sessions: Vec<(i64, NaiveDate)>,
    /// Block weeks that got no session the FTP detector would count, because
    /// the library holds nothing that qualifies. Reported, never hidden.
    pub weeks_without_hard_evidence: Vec<u32>,
}

/// The Monday the next block should open on.
///
/// The Monday after the finished program's last day, but never earlier than
/// next Monday: a rider who opens the app in November after a block that ended
/// in September must not be handed one written into their past.
///
/// A week every one of their training days falls inside is skipped whole. A
/// *single* day off is a different problem with a different answer —
/// [`crate::ai::context::drop_time_off_days`] removes that one session at write
/// time — but opening a block into a week the rider is away for spends its
/// first week on nothing and puts the real work in what the plan calls week two.
pub fn next_block_monday(
    finished: &Program,
    today: NaiveDate,
    training_days: &[Weekday],
    time_off: &HashSet<NaiveDate>,
) -> Result<NaiveDate, RolloverError> {
    if training_days.is_empty() {
        return Err(RolloverError::NoTrainingDays);
    }
    let after_program = next_monday(last_day(finished));
    let mut monday = after_program.max(next_monday(today));

    for _ in 0..MAX_START_SEARCH_WEEKS {
        let all_off = training_days.iter().all(|d| {
            time_off.contains(&(monday + Duration::days(d.num_days_from_monday() as i64)))
        });
        if !all_off {
            return Ok(monday);
        }
        monday += Duration::days(7);
    }
    Err(RolloverError::AllTimeOff)
}

/// The Monday strictly after `date`.
fn next_monday(date: NaiveDate) -> NaiveDate {
    date + Duration::days(7 - date.weekday().num_days_from_monday() as i64)
}

/// Whether the block should open on its easy week.
///
/// The cycle the finished program was running matters, but so does what has
/// happened since it stopped. A rider who ended on a building week is owed a
/// recovery one — unless the gap before the new block already gave them the
/// rest, which is exactly what a week away does. Handing someone a recovery
/// week on the Monday they get back from six days off is the plan failing to
/// notice they have already had it.
pub fn opens_easy(
    finished: &Program,
    start_monday: NaiveDate,
    time_off: &HashSet<NaiveDate>,
) -> bool {
    if phase_of(finished.num_weeks.max(1)) == Phase::Recovery {
        return false;
    }
    let rested = (1..=7)
        .filter(|n| time_off.contains(&(start_monday - Duration::days(*n))))
        .count();
    rested < REST_DAYS_FOR_A_TAKEN_BREAK
}

/// Where a week of the block sits in the build/recover cycle, 1-based.
///
/// The block numbers from one, like the program row it will be stored as, so
/// [`phase_of`] reads it directly. `easy_first` rotates the deload to the front
/// for a block that owes one.
///
/// **Phase and volume both read this.** They were two expressions once, and
/// they came apart the first time a block opened easy: the deload moved to week
/// one while the volume ramp stayed where it was, so week one was a recovery
/// week at full length and week four a threshold week at deload length. One
/// position, one answer.
fn cycle_position(week: u32, easy_first: bool) -> u32 {
    if easy_first {
        // Rotate by one so week 1 is the deload and the building weeks follow.
        ((week + BLOCK_WEEKS - 2) % BLOCK_WEEKS) + 1
    } else {
        ((week - 1) % BLOCK_WEEKS) + 1
    }
}

/// How many sessions of real intensity a week should carry.
///
/// **Zero in a recovery week** — the easy week is easy on purpose, and
/// [`crate::training::program::suggest`] already refuses to adjust one, so a
/// hard session placed there would be beyond the reach of the rules that exist
/// to protect the rider from it.
///
/// One in a building week, two once there are four or more days to spread them
/// across. Three building weeks at one apiece is
/// [`crate::training::ftp_detect`]'s three-session minimum inside its own
/// four-week window: the floor is set by what it takes for the app to be able
/// to tell the rider their FTP has moved. The block that just finished had
/// none — its hardest work was sweet spot at exactly 90 % of FTP, one point
/// under the bar — which is why it could never have produced that answer.
fn hard_target(slots: usize, phase: Phase) -> usize {
    match phase {
        Phase::Recovery => 0,
        Phase::Build if slots >= 4 => 2,
        Phase::Build => 1,
    }
}

/// Which slots in the week take the hard sessions.
///
/// Spread as far apart as the week allows, counting the wrap from Sunday back
/// to Monday as adjacency — two hard days either side of a week boundary are
/// back to back however the calendar is drawn. Exhaustive over at most 21
/// pairs, and tie-broken toward the earlier one, so the answer is the same
/// every time it is asked.
fn hard_slots(days: &[Weekday], want: usize) -> Vec<usize> {
    let n = days.len();
    if want == 0 || n == 0 {
        return Vec::new();
    }
    if want == 1 {
        return vec![n / 2];
    }
    let mut best: Option<(i64, usize, usize)> = None;
    for a in 0..n {
        for b in (a + 1)..n {
            let fwd = days[b].num_days_from_monday() as i64 - days[a].num_days_from_monday() as i64;
            let gap = fwd.min(7 - fwd);
            if best.is_none_or(|(g, _, _)| gap > g) {
                best = Some((gap, a, b));
            }
        }
    }
    best.map(|(_, a, b)| vec![a, b]).unwrap_or_default()
}

/// The typical length of a session in the block that just finished.
///
/// The median rather than the mean, so one long day does not set the next
/// block's shape. Sessions the rider actually rode are asked first: what they
/// completed says more about the time they have than what was written for them.
fn baseline_secs(finished: &[PlannedSession]) -> u32 {
    let median = |mut v: Vec<u32>| -> Option<u32> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2])
    };
    median(
        finished
            .iter()
            .filter(|s| s.completed)
            .map(|s| s.duration_secs)
            .collect(),
    )
    .or_else(|| median(finished.iter().map(|s| s.duration_secs).collect()))
    .unwrap_or(FALLBACK_SESSION_SECS)
}

/// The hard workout closest to `duration_secs` that carries enough work above
/// the detector's threshold to actually be counted.
///
/// Filters on what a workout *prescribes*, not on what its category is called.
/// A "Threshold" session written at 90 % of FTP is a label away from being
/// evidence and one whole watt short of being it, and a block full of those
/// would look like it was working while telling the FTP check-in nothing.
fn pick_hard<'a>(
    library: &'a [Workout],
    duration_secs: u32,
    ftp: u32,
    prefer: WorkoutCategory,
    used: &HashSet<i64>,
) -> Option<&'a Workout> {
    let qualifies = |w: &&Workout| hard_evidence_seconds(w, ftp) >= MIN_HARD_SECONDS;
    let closest = |cat: Option<WorkoutCategory>, fresh: bool| {
        library
            .iter()
            .filter(|w| !fresh || !used.contains(&w.id))
            .filter(|w| cat.is_none_or(|c| w.category == c))
            .filter(qualifies)
            .min_by_key(|w| w.duration_secs.abs_diff(duration_secs))
    };
    closest(Some(prefer), true)
        .or_else(|| closest(None, true))
        .or_else(|| closest(Some(prefer), false))
        .or_else(|| closest(None, false))
}

/// The session length a week of the block aims at, by its cycle position.
///
/// Separated out so the progression is one testable expression rather than
/// something to be inferred from which workouts happened to be picked — with
/// variety in play, two weeks aiming at different lengths can still land on
/// neighbouring rows in a thin library.
fn week_target(baseline: u32, position: u32) -> u32 {
    (baseline as f32 * RAMP[(position - 1) as usize % RAMP.len()]) as u32
}

/// [`pick_replacement`], but preferring a workout the week has not used yet.
///
/// Without this a three-day recovery week is the same ride three times: the
/// picker is deterministic and length-matching, so every slot of the same
/// category and target resolves to the same row. Repeating is still allowed
/// when the library genuinely holds nothing else — a thin library should give a
/// repeated session rather than a missing one.
fn pick_varied<'a>(
    library: &'a [Workout],
    category: WorkoutCategory,
    duration_secs: u32,
    used: &HashSet<i64>,
) -> Option<&'a Workout> {
    let unused: Vec<Workout> = library
        .iter()
        .filter(|w| !used.contains(&w.id))
        .cloned()
        .collect();
    pick_replacement(&unused, category, duration_secs)
        .map(|w| w.id)
        .and_then(|id| library.iter().find(|w| w.id == id))
        .or_else(|| pick_replacement(library, category, duration_secs))
}

/// Build the next block from the one that just finished.
///
/// `training_days` comes from the rider, never from the finished plan: the
/// weekdays a program's entries land on say almost nothing once sessions have
/// been dragged around for fifteen weeks — the live block's 26 entries touch
/// all seven — and a block built from that union would prescribe daily
/// training. The dialog asks; this takes the answer.
pub fn next_block(
    finished: &[PlannedSession],
    library: &[Workout],
    start_monday: NaiveDate,
    training_days: &[Weekday],
    ftp: u32,
    easy_first: bool,
) -> Result<NextBlock, RolloverError> {
    if training_days.is_empty() {
        return Err(RolloverError::NoTrainingDays);
    }
    if library.is_empty() {
        return Err(RolloverError::EmptyLibrary);
    }

    let mut days: Vec<Weekday> = training_days.to_vec();
    days.sort_by_key(|d| d.num_days_from_monday());
    days.dedup();

    let baseline = baseline_secs(finished);
    let mut sessions = Vec::new();
    let mut weeks_without_hard_evidence = Vec::new();

    for week in 1..=BLOCK_WEEKS {
        let position = cycle_position(week, easy_first);
        let phase = phase_of(position);
        let target = week_target(baseline, position);
        let hard = hard_slots(&days, hard_target(days.len(), phase));
        let mut hard_found = 0usize;
        // Reset each week: variety within a week is what stops a deload reading
        // as the same ride three times, and a fortnight apart is not repetition.
        let mut used: HashSet<i64> = HashSet::new();

        for (slot, day) in days.iter().enumerate() {
            let date = start_monday
                + Duration::days((week as i64 - 1) * 7 + day.num_days_from_monday() as i64);

            let picked = if let Some(rank) = hard.iter().position(|s| *s == slot) {
                // The first hard day of a week is the threshold one; a second
                // reaches higher, so the week has one of each rather than two
                // of the same.
                let prefer = if rank == 0 {
                    WorkoutCategory::Threshold
                } else {
                    WorkoutCategory::Vo2Max
                };
                match pick_hard(library, target, ftp, prefer, &used) {
                    Some(w) => {
                        hard_found += 1;
                        Some(w)
                    }
                    // Nothing in the library qualifies. Sweet spot is the
                    // honest substitute — it is the hardest thing that can be
                    // asked for — and the week is reported rather than quietly
                    // filled.
                    None => pick_varied(library, WorkoutCategory::SweetSpot, target, &used),
                }
            } else if phase == Phase::Recovery {
                pick_varied(library, WorkoutCategory::Recovery, target, &used)
                    .or_else(|| pick_varied(library, WorkoutCategory::Endurance, target, &used))
            } else {
                pick_varied(library, WorkoutCategory::Endurance, target, &used)
            };

            if let Some(w) = picked {
                used.insert(w.id);
                sessions.push((w.id, date));
            }
        }

        if hard_target(days.len(), phase) > 0 && hard_found == 0 {
            weeks_without_hard_evidence.push(week);
        }
    }

    sessions.sort_by_key(|(_, d)| *d);
    Ok(NextBlock {
        start_monday,
        weeks: BLOCK_WEEKS,
        sessions,
        weeks_without_hard_evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;

    const FTP: u32 = 200;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    fn program(weeks: u32) -> Program {
        Program {
            id: 1,
            // Monday 15 June 2026 — the live program's own start.
            start_monday: date(2026, 6, 15),
            num_weeks: weeks,
            training_days: String::new(),
        }
    }

    fn workout(id: i64, name: &str, cat: WorkoutCategory, mins: u32, pct: f32) -> Workout {
        Workout {
            id,
            name: name.into(),
            description: String::new(),
            duration_secs: mins * 60,
            tss: 50.0,
            category: cat,
            segments: vec![
                Segment::steady(600, 55.0, "Warm-up"),
                Segment::steady(mins * 60 - 600, pct, "Main"),
            ],
        }
    }

    /// A library shaped like the rider's: real threshold and VO2 work, sweet
    /// spot prescribed at exactly 90 %, and easy days to fill the rest.
    fn library() -> Vec<Workout> {
        vec![
            workout(1, "Recovery 45", WorkoutCategory::Recovery, 45, 50.0),
            workout(2, "Endurance 60", WorkoutCategory::Endurance, 60, 65.0),
            workout(3, "Endurance 75", WorkoutCategory::Endurance, 75, 65.0),
            workout(4, "Sweet Spot 2x15", WorkoutCategory::SweetSpot, 55, 90.0),
            workout(5, "Threshold 2x15", WorkoutCategory::Threshold, 60, 100.0),
            workout(6, "VO2 4x4", WorkoutCategory::Vo2Max, 55, 115.0),
        ]
    }

    fn session(d: NaiveDate, mins: u32, completed: bool) -> PlannedSession {
        PlannedSession {
            trained: false,
            entry_id: 1,
            date: d,
            workout_id: 1,
            workout_name: "Session".into(),
            category: WorkoutCategory::Endurance,
            tss: 50.0,
            duration_secs: mins * 60,
            completed,
            adjusted_from: None,
            previous_step_name: None,
        }
    }

    fn mwf() -> Vec<Weekday> {
        vec![Weekday::Mon, Weekday::Wed, Weekday::Fri]
    }

    fn block(days: &[Weekday], easy_first: bool) -> NextBlock {
        next_block(
            &[session(date(2026, 9, 7), 60, true)],
            &library(),
            date(2026, 9, 28),
            days,
            FTP,
            easy_first,
        )
        .expect("a block from a full library")
    }

    /// The category each session in `week` was given.
    fn categories(b: &NextBlock, week: u32) -> Vec<WorkoutCategory> {
        let lib = library();
        let from = b.start_monday + Duration::days((week as i64 - 1) * 7);
        b.sessions
            .iter()
            .filter(|(_, d)| *d >= from && *d < from + Duration::days(7))
            .map(|(id, _)| {
                lib.iter()
                    .find(|w| w.id == *id)
                    .expect("a workout from the library")
                    .category
            })
            .collect()
    }

    fn hard_count(b: &NextBlock, week: u32) -> usize {
        categories(b, week)
            .iter()
            .filter(|c| crate::training::program::is_hard(**c))
            .count()
    }

    // ── Where the hard work goes ──────────────────────────────────────────────

    #[test]
    fn should_prescribe_a_hard_session_in_every_build_week() {
        let b = block(&mwf(), false);
        for week in 1..=3 {
            assert!(
                hard_count(&b, week) >= 1,
                "week {week} carried no hard session"
            );
        }
    }

    #[test]
    fn should_prescribe_no_hard_session_in_the_recovery_week() {
        let b = block(&mwf(), false);
        assert_eq!(hard_count(&b, 4), 0);
    }

    #[test]
    fn should_hold_the_three_hard_sessions_the_ftp_detector_needs() {
        // Three building weeks at one apiece is exactly MIN_HARD_SESSIONS
        // inside the detector's own 28-day window.
        let b = block(&mwf(), false);
        let total: usize = (1..=BLOCK_WEEKS).map(|w| hard_count(&b, w)).sum();
        assert!(total >= crate::training::ftp_detect::MIN_HARD_SESSIONS);
    }

    #[test]
    fn should_prescribe_two_hard_sessions_when_there_are_four_days_to_spread_them() {
        let days = vec![Weekday::Mon, Weekday::Tue, Weekday::Thu, Weekday::Sat];
        assert_eq!(hard_count(&block(&days, false), 1), 2);
    }

    #[test]
    fn should_never_put_two_hard_sessions_on_consecutive_days() {
        let days = vec![Weekday::Mon, Weekday::Tue, Weekday::Thu, Weekday::Sat];
        let picked = hard_slots(&days, 2);
        let gap = (days[picked[1]].num_days_from_monday() as i64)
            - (days[picked[0]].num_days_from_monday() as i64);
        assert!(gap.min(7 - gap) >= 2, "hard days landed back to back");
    }

    #[test]
    fn should_treat_sunday_and_monday_as_adjacent() {
        // Sun and Mon are one day apart across the week boundary, so the pair
        // chosen must not be that one.
        let days = vec![Weekday::Mon, Weekday::Wed, Weekday::Sun];
        let picked = hard_slots(&days, 2);
        assert_ne!(
            (picked[0], picked[1]),
            (0, 2),
            "Monday and Sunday are back to back"
        );
    }

    #[test]
    fn should_not_count_a_sweet_spot_library_as_hard_evidence() {
        // Everything at 90 % of FTP: no session can qualify, and the block says
        // so rather than pretending.
        let lib = vec![
            workout(1, "Sweet Spot", WorkoutCategory::SweetSpot, 55, 90.0),
            workout(2, "Endurance", WorkoutCategory::Endurance, 60, 65.0),
        ];
        let b = next_block(&[], &lib, date(2026, 9, 28), &mwf(), FTP, false)
            .expect("a block, even a compromised one");
        assert_eq!(b.weeks_without_hard_evidence, vec![1, 2, 3]);
    }

    // ── Volume ────────────────────────────────────────────────────────────────

    #[test]
    fn should_grow_the_target_across_the_building_weeks() {
        let base = 3600;
        assert!(week_target(base, 2) > week_target(base, 1));
        assert!(week_target(base, 3) > week_target(base, 2));
    }

    #[test]
    fn should_cut_the_target_in_the_recovery_week() {
        let base = 3600;
        assert!(
            week_target(base, 4) < week_target(base, 1),
            "the fourth position is the easy one"
        );
    }

    #[test]
    fn should_keep_volume_and_phase_together_when_the_block_opens_easy() {
        // The bug a real run found: the deload moved to week one while the
        // volume ramp stayed put, so week one was a recovery week at full
        // length and week four a threshold week at deload length.
        let base = 3600;
        for easy_first in [false, true] {
            for week in 1..=BLOCK_WEEKS {
                let pos = cycle_position(week, easy_first);
                let light = week_target(base, pos) < week_target(base, 1);
                if phase_of(pos) == Phase::Recovery {
                    assert!(
                        light,
                        "easy_first={easy_first} week {week}: recovery week is not the light one"
                    );
                }
            }
        }
    }

    #[test]
    fn should_put_the_deload_in_week_one_when_the_block_opens_easy() {
        assert_eq!(cycle_position(1, true), BLOCK_WEEKS);
        assert_eq!(cycle_position(2, true), 1);
        assert_eq!(cycle_position(3, true), 2);
        assert_eq!(cycle_position(4, true), 3);
    }

    #[test]
    fn should_run_the_cycle_straight_through_when_nothing_is_owed() {
        for week in 1..=BLOCK_WEEKS {
            assert_eq!(cycle_position(week, false), week);
        }
    }

    #[test]
    fn should_end_a_block_that_opened_easy_on_a_building_week() {
        // Deliberate: the deload was taken up front, so the block finishes on
        // work and the next roll-over is the one that owes the rest.
        let b = block(&mwf(), true);
        assert_eq!(hard_count(&b, 1), 0);
        assert!(hard_count(&b, 4) >= 1);
    }

    #[test]
    fn should_take_its_length_from_the_sessions_that_were_actually_ridden() {
        let finished = vec![
            session(date(2026, 9, 1), 45, true),
            session(date(2026, 9, 3), 45, true),
            // Written but never ridden, and three times as long — it must not
            // set the next block's shape.
            session(date(2026, 9, 5), 180, false),
        ];
        assert_eq!(baseline_secs(&finished), 45 * 60);
    }

    #[test]
    fn should_fall_back_to_the_planned_lengths_when_nothing_was_ridden() {
        let finished = vec![session(date(2026, 9, 1), 90, false)];
        assert_eq!(baseline_secs(&finished), 90 * 60);
    }

    #[test]
    fn should_fall_back_to_an_hour_when_the_old_block_says_nothing() {
        assert_eq!(baseline_secs(&[]), FALLBACK_SESSION_SECS);
    }

    // ── Shape ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_not_prescribe_the_same_ride_twice_in_one_week() {
        // A three-day recovery week used to resolve to the same row three
        // times: the picker is deterministic, so every slot of one category and
        // target lands on one workout.
        let lib = vec![
            workout(1, "Recovery 45", WorkoutCategory::Recovery, 45, 50.0),
            workout(2, "Easy Spin", WorkoutCategory::Recovery, 45, 50.0),
            workout(3, "Gentle Pedaling", WorkoutCategory::Recovery, 50, 50.0),
            workout(4, "Endurance 60", WorkoutCategory::Endurance, 60, 65.0),
            workout(5, "Threshold 2x15", WorkoutCategory::Threshold, 60, 100.0),
        ];
        let b = next_block(&[], &lib, date(2026, 9, 28), &mwf(), FTP, false).expect("a block");

        let from = b.start_monday + Duration::days(3 * 7);
        let mut week4: Vec<i64> = b
            .sessions
            .iter()
            .filter(|(_, d)| *d >= from)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(week4.len(), 3);
        week4.sort_unstable();
        week4.dedup();
        assert_eq!(week4.len(), 3, "the recovery week repeated a ride");
    }

    #[test]
    fn should_repeat_rather_than_leave_a_day_empty_when_the_library_is_thin() {
        // One recovery ride in the whole library: a repeated session beats a
        // missing one.
        let lib = vec![
            workout(1, "Recovery 45", WorkoutCategory::Recovery, 45, 50.0),
            workout(2, "Endurance 60", WorkoutCategory::Endurance, 60, 65.0),
            workout(3, "Threshold 2x15", WorkoutCategory::Threshold, 60, 100.0),
        ];
        let b = next_block(&[], &lib, date(2026, 9, 28), &mwf(), FTP, false).expect("a block");
        assert_eq!(b.sessions.len(), 3 * BLOCK_WEEKS as usize);
    }

    #[test]
    fn should_place_sessions_only_on_the_chosen_training_days() {
        let b = block(&mwf(), false);
        for (_, d) in &b.sessions {
            assert!(
                mwf().contains(&d.weekday()),
                "{d} is not one of the chosen days"
            );
        }
    }

    #[test]
    fn should_run_for_a_whole_block_of_weeks() {
        let b = block(&mwf(), false);
        assert_eq!(b.weeks, BLOCK_WEEKS);
        assert_eq!(b.sessions.len(), 3 * BLOCK_WEEKS as usize);
    }

    #[test]
    fn should_refuse_a_block_when_no_training_day_was_chosen() {
        let r = next_block(&[], &library(), date(2026, 9, 28), &[], FTP, false);
        assert_eq!(r, Err(RolloverError::NoTrainingDays));
    }

    #[test]
    fn should_refuse_a_block_when_the_library_is_empty() {
        let r = next_block(&[], &[], date(2026, 9, 28), &mwf(), FTP, false);
        assert_eq!(r, Err(RolloverError::EmptyLibrary));
    }

    #[test]
    fn should_open_on_the_easy_week_when_it_is_owed_one() {
        let b = block(&mwf(), true);
        assert_eq!(hard_count(&b, 1), 0, "the opening week is the easy one");
        assert!(hard_count(&b, 2) >= 1);
    }

    // ── When the block opens ──────────────────────────────────────────────────

    fn cairo() -> HashSet<NaiveDate> {
        (23..=28).map(|d| date(2026, 9, d)).collect()
    }

    #[test]
    fn should_open_on_the_monday_after_the_program_ends() {
        // 15 June + 15 weeks closes Sunday 27 September.
        let m = next_block_monday(&program(15), date(2026, 9, 8), &mwf(), &HashSet::new());
        assert_eq!(m, Ok(date(2026, 9, 28)));
    }

    #[test]
    fn should_keep_the_opening_week_when_only_one_of_its_days_is_time_off() {
        // Cairo covers Monday 28 September, but Wednesday and Friday are clear:
        // the block opens, and the write-time filter drops the Monday.
        let m = next_block_monday(&program(15), date(2026, 9, 8), &mwf(), &cairo());
        assert_eq!(m, Ok(date(2026, 9, 28)));
    }

    #[test]
    fn should_wait_a_week_when_every_training_day_of_the_first_one_is_off() {
        let off: HashSet<NaiveDate> = (28..=30)
            .map(|d| date(2026, 9, d))
            .chain((1..=4).map(|d| date(2026, 10, d)))
            .collect();
        let m = next_block_monday(&program(15), date(2026, 9, 8), &mwf(), &off);
        assert_eq!(m, Ok(date(2026, 10, 5)));
    }

    #[test]
    fn should_never_open_a_block_in_the_past() {
        // The rider comes back in December to a program that ended in September.
        let m = next_block_monday(&program(15), date(2026, 12, 3), &mwf(), &HashSet::new());
        assert_eq!(m, Ok(date(2026, 12, 7)), "next Monday, not last September");
    }

    #[test]
    fn should_refuse_when_every_week_it_can_reach_is_time_off() {
        let off: HashSet<NaiveDate> = (0..120)
            .map(|n| date(2026, 9, 28) + Duration::days(n))
            .collect();
        let m = next_block_monday(&program(15), date(2026, 9, 8), &mwf(), &off);
        assert_eq!(m, Err(RolloverError::AllTimeOff));
    }

    // ── Whether the easy week is owed ─────────────────────────────────────────

    #[test]
    fn should_owe_an_easy_week_when_the_last_one_was_a_building_week() {
        // 15 weeks is three into a block, so the cycle owes the deload.
        assert!(opens_easy(&program(15), date(2026, 9, 28), &HashSet::new()));
    }

    #[test]
    fn should_owe_nothing_when_the_program_ended_on_its_own_recovery_week() {
        assert!(!opens_easy(
            &program(16),
            date(2026, 10, 5),
            &HashSet::new()
        ));
    }

    #[test]
    fn should_not_owe_an_easy_week_after_a_week_away() {
        // Six days off in Cairo is the recovery week, already taken. Opening
        // the block with another one would be the plan failing to notice.
        assert!(!opens_easy(&program(15), date(2026, 9, 28), &cairo()));
    }

    #[test]
    fn should_still_owe_an_easy_week_after_only_a_long_weekend() {
        let off: HashSet<NaiveDate> = (25..=27).map(|d| date(2026, 9, d)).collect();
        assert!(opens_easy(&program(15), date(2026, 9, 28), &off));
    }
}
