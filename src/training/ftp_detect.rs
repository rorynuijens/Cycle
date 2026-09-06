//! Does the evidence say the rider's FTP is wrong?
//!
//! Classic FTP estimation fits a curve over best maximal efforts. ERG mode
//! invalidates that for an indoor rider: the trainer holds power at whatever the
//! workout asked for, so the power curve records what the *plan* demanded and
//! never sees the rider's ceiling move. Detection here leans on completion
//! quality instead — how closely hard segments were held, whether any of them
//! fell apart, what the ride felt like (RPE), and how heart rate drifted through
//! the steady ones. The full design is in `docs/ftp-detection.md`.
//!
//! Nothing here changes anything. The output is a suggestion with its evidence
//! spelled out in sentences, for a card that the rider accepts or dismisses.
//!
//! Two departures from the spec, both forced by what the recorded rides actually
//! contain (measured 2026-09-06 against the live database):
//!
//! * The spec's only failure signal is the ERG "spiral of death" — cadence
//!   collapsing while power falls short. Every session recorded so far reports
//!   cadence 0 for every second, because no cadence sensor is feeding the app
//!   yet. `cadence < 0.8 × median` where the median is zero can never be true,
//!   so no segment could ever fail, `fail_rate` would be pinned at 0, and the
//!   only reachable rule would be the one that raises FTP. A power-only
//!   shortfall rule ([`SHORTFALL_SECONDS`]) gives the detector a way to see a
//!   failure without cadence.
//! * Even so, a window with no cadence anywhere cannot see the spiral, which is
//!   the sharper signal: in ERG the cadence goes first and power follows. So an
//!   *up* suggestion is withheld entirely until some hard session in the window
//!   recorded cadence. Down and "looks right" still work — a detector that can
//!   only ease, when it is half blind, errs in the safe direction.

// The check-in card (docs/ftp-detection.md §6) is the caller, and lands next.
// Everything here is deliberately caller-less until then, in the same way the
// phase-1 `ftp_history` CRUD was.
#![allow(dead_code)]

use chrono::{Local, NaiveDate};

use crate::data::session::{DataPoint, Session};

/// Length of an analysis window, in days. Two are used: the most recent window
/// supplies the evidence, the one before it the heart-rate-drift baseline.
const WINDOW_DAYS: i64 = 28;

/// A target at or above this percentage of ride-time FTP counts as hard.
///
/// It is the Z4 boundary used by `power_zone_index`, so what the detector calls
/// threshold work is what the rest of the app colours as threshold work.
/// Compared as integer percentages throughout: `0.91_f32 * 200.0` is 182.000005,
/// which would exclude a target of exactly 182 W from its own zone.
const HARD_TARGET_PCT: u32 = 91;

/// Hard seconds a session needs before it counts as evidence rather than noise.
const MIN_HARD_SECONDS: u32 = 600;

/// Hard-evidence sessions the window needs before any suggestion is made.
const MIN_HARD_SESSIONS: usize = 3;

/// Cadence this far below the session's pedalling median is a collapse.
const STRUGGLE_CADENCE_FRACTION: f32 = 0.8;
/// Power below this percentage of target, while cadence has collapsed, is a
/// segment coming apart rather than ordinary ERG wobble.
const STRUGGLE_POWER_PCT: u32 = 95;
/// Seconds of collapsed cadence and short power that fail a segment.
const STRUGGLE_SECONDS: usize = 10;

/// Power below this percentage of target for [`SHORTFALL_SECONDS`] fails a
/// segment on power alone. Deliberately further from target, and held for
/// longer, than the cadence rule: without cadence to corroborate it, the
/// evidence has to be unambiguous before it is called a failure.
const SHORTFALL_POWER_PCT: u32 = 90;
/// Seconds of sustained shortfall that fail a segment with no cadence data.
const SHORTFALL_SECONDS: usize = 30;

/// A ride that stopped short of this fraction of its plan was abandoned.
const ABANDONED_FRACTION: f32 = 0.9;

/// Shortest hard segment that can yield a heart-rate drift figure.
const DRIFT_MIN_SECONDS: usize = 480;
/// A segment is "steady" enough to read drift from when its target varies by no
/// more than this fraction of its mean.
const STEADY_TARGET_TOLERANCE: f32 = 0.05;
/// Drift falling by this many percentage points across windows counts as an
/// improvement worth a point of extra confidence.
const DRIFT_IMPROVEMENT_PP: f32 = 2.0;

/// Failed share of hard segments that calls for easing FTP.
const DOWN_FAIL_RATE: f32 = 0.25;
/// RPE at or above this, with some failure, also calls for easing.
const DOWN_RPE: f32 = 8.5;
/// The failure share required alongside a high RPE.
const DOWN_RPE_FAIL_RATE: f32 = 0.10;
/// Duration-weighted compliance a clean window needs to justify a rise.
const UP_COMPLIANCE: f32 = 0.98;
/// RPE at or below this is what "comfortably held" looks like.
const UP_RPE: f32 = 6.0;
/// RPE at or below this is easy enough to be worth an extra point.
const UP_RPE_EASY: f32 = 5.0;
/// RPE ceiling for trusting an external eFTP over the rider's own number.
const CROSS_CHECK_RPE: f32 = 6.5;
/// An external eFTP must exceed the stored FTP by this factor to be believed.
/// How far it is then trusted in one step is the ordinary [`MAX_DELTA_PCT`]
/// guard rail — the spec's separate 1.05 cap is the same 5 %.
const CROSS_CHECK_MARGIN: f32 = 1.03;

/// Largest change a single suggestion may propose, in percent.
const MAX_DELTA_PCT: f32 = 5.0;
/// Plausible bounds for a suggested FTP (CLAUDE.md §5.2 — stored data is not
/// trusted blindly).
const MIN_FTP_WATTS: u32 = 50;
const MAX_FTP_WATTS: u32 = 500;

/// Days after any FTP change before a rise may be suggested.
const UP_COOLDOWN_DAYS: i64 = 21;
/// Days before a drop may be suggested. Shorter than the rise cooldown on
/// purpose: protecting the rider from an FTP set too high beats rhythm.
const DOWN_COOLDOWN_DAYS: i64 = 7;

/// Why a hard segment was judged to have failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Cadence collapsed while power fell short — the ERG "spiral of death".
    CadenceCollapse,
    /// Power stayed well under target long enough to count on its own.
    PowerShortfall,
    /// The ride was abandoned inside this segment.
    StoppedEarly,
}

/// One contiguous run of seconds spent at a threshold-or-above target.
#[derive(Debug, Clone, PartialEq)]
pub struct HardSegment {
    /// Offset of the segment's first second into the ride.
    pub start_secs: u32,
    pub duration_secs: u32,
    pub mean_power: f32,
    pub mean_target: f32,
    /// `mean_power / mean_target` — 1.0 is exactly on target.
    pub compliance: f32,
    /// Heart-rate drift (second half / first half), for long steady segments.
    pub drift: Option<f32>,
    pub failure: Option<Failure>,
}

impl HardSegment {
    pub fn failed(&self) -> bool {
        self.failure.is_some()
    }
}

/// What one ride contributes to the window.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvidence {
    pub date: NaiveDate,
    /// FTP as it stood when the ride was recorded, not as it stands now.
    pub ftp_watts: u32,
    pub rpe: Option<u8>,
    pub segments: Vec<HardSegment>,
    /// Whether any second of the ride recorded a cadence above zero. An absent
    /// sensor reports a flat zero rather than nothing at all, so `Some(0)`
    /// everywhere has to read as "no cadence" — see the module note.
    pub cadence_present: bool,
}

impl SessionEvidence {
    pub fn hard_seconds(&self) -> u32 {
        self.segments.iter().map(|s| s.duration_secs).sum()
    }

    /// Whether this ride carries enough threshold work to reason from.
    pub fn is_hard_evidence(&self) -> bool {
        self.hard_seconds() >= MIN_HARD_SECONDS
    }

    /// Mean drift across the segments long and steady enough to yield one.
    fn mean_drift(&self) -> Option<f32> {
        let drifts: Vec<f32> = self.segments.iter().filter_map(|s| s.drift).collect();
        if drifts.is_empty() {
            return None;
        }
        Some(drifts.iter().sum::<f32>() / drifts.len() as f32)
    }
}

/// The window's evidence, reduced to the figures the rules are written against.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSummary {
    /// Hard-evidence sessions in the most recent window.
    pub n_hard: usize,
    pub total_segments: usize,
    pub failed_segments: usize,
    /// Failed share of hard segments, 0.0 when there are none.
    pub fail_rate: f32,
    /// Duration-weighted `power / target` across hard segments.
    pub avg_compliance: f32,
    /// Mean RPE over hard-evidence sessions, needing at least two to mean much.
    pub avg_rpe_hard: Option<f32>,
    /// Percentage points by which drift *improved* against the previous window.
    /// Positive means heart rate held steadier for the same work.
    pub drift_trend: Option<f32>,
    /// Whether any hard-evidence session in the window recorded cadence.
    pub cadence_present: bool,
}

/// Which way a suggestion moves the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    /// The evidence supports the FTP the rider already has.
    Hold,
}

/// A proposed FTP, with the reasoning that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct FtpSuggestion {
    pub new_ftp: u32,
    /// Signed change from the current FTP, in percent.
    pub delta_pct: f32,
    pub direction: Direction,
    /// One human sentence per piece of reasoning, shown on the check-in card.
    pub evidence: Vec<String>,
}

// ── Evidence extraction ─────────────────────────────────────────────────────

/// Reduce one recorded ride to its evidence, or `None` when it has none to give.
///
/// A ride is unusable when it carries no ride-time FTP or no recorded targets:
/// route rides and imported files never had targets, and rides recorded before
/// target capture shipped (2026-07-24) lost theirs. `planned_duration_secs` is
/// the workout's planned length where there is one, used only to notice a ride
/// abandoned part-way through a hard effort.
///
/// Each recorded point is treated as one second, as elsewhere in the app — the
/// player records at 1 Hz.
pub fn session_evidence(
    session: &Session,
    planned_duration_secs: Option<u32>,
) -> Option<SessionEvidence> {
    let ftp = session.ftp_watts?;
    if ftp == 0 {
        return None;
    }
    let points = &session.data_points;
    if !points.iter().any(|p| p.target_watts.is_some()) {
        return None;
    }

    let median_cadence = pedalling_median_cadence(points);
    let abandoned = planned_duration_secs
        .is_some_and(|planned| (points.len() as f32) < ABANDONED_FRACTION * planned as f32);

    let runs = hard_runs(points, ftp);
    let mut segments = Vec::with_capacity(runs.len());
    for (start, end) in runs {
        let run = &points[start..end];
        // A ride cut short during a hard effort is a failure of that effort,
        // whatever the numbers up to the moment it stopped were saying.
        let stopped_here = abandoned && end == points.len();
        let failure = if stopped_here {
            Some(Failure::StoppedEarly)
        } else {
            segment_failure(run, median_cadence)
        };
        let mean_power = mean_of(run.iter().filter_map(|p| p.power_watts));
        let mean_target = mean_of(run.iter().filter_map(|p| p.target_watts));
        segments.push(HardSegment {
            start_secs: points[start].elapsed_secs,
            duration_secs: run.len() as u32,
            mean_power,
            mean_target,
            compliance: if mean_target > 0.0 {
                mean_power / mean_target
            } else {
                0.0
            },
            drift: segment_drift(run),
            failure,
        });
    }

    Some(SessionEvidence {
        date: session.started_at.with_timezone(&Local).date_naive(),
        ftp_watts: ftp,
        rpe: session.rpe,
        segments,
        cadence_present: median_cadence.is_some(),
    })
}

fn mean_of(values: impl Iterator<Item = u32>) -> f32 {
    let mut sum = 0u64;
    let mut count = 0u32;
    for v in values {
        sum += v as u64;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        sum as f32 / count as f32
    }
}

/// Median of the seconds the rider was actually turning the pedals.
///
/// Zeros are excluded rather than averaged in: coasting through a recovery
/// valley would otherwise drag the median down and desensitise the collapse
/// rule. It also means an absent sensor — which reports a flat zero — yields
/// `None`, which is how the rest of the module tells the two apart.
fn pedalling_median_cadence(points: &[DataPoint]) -> Option<f32> {
    let mut pedalling: Vec<u32> = points
        .iter()
        .filter_map(|p| p.cadence_rpm)
        .filter(|&c| c > 0)
        .collect();
    if pedalling.is_empty() {
        return None;
    }
    pedalling.sort_unstable();
    let mid = pedalling.len() / 2;
    Some(if pedalling.len().is_multiple_of(2) {
        (pedalling[mid - 1] + pedalling[mid]) as f32 / 2.0
    } else {
        pedalling[mid] as f32
    })
}

/// Index ranges of the contiguous runs spent at a hard target.
fn hard_runs(points: &[DataPoint], ftp: u32) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, p) in points.iter().enumerate() {
        let hard = p
            .target_watts
            .is_some_and(|t| t * 100 >= ftp * HARD_TARGET_PCT);
        match (hard, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, points.len()));
    }
    runs
}

/// Judge one hard segment, preferring the cadence signal where there is one.
///
/// A second with no power reading breaks both runs rather than extending them:
/// a sensor dropout is missing evidence, not evidence of failure.
fn segment_failure(run: &[DataPoint], median_cadence: Option<f32>) -> Option<Failure> {
    let mut collapsed = 0usize;
    let mut short = 0usize;
    for p in run {
        let (Some(power), Some(target)) = (p.power_watts, p.target_watts) else {
            collapsed = 0;
            short = 0;
            continue;
        };
        if let Some(median) = median_cadence {
            let cadence_down = p
                .cadence_rpm
                .is_some_and(|c| (c as f32) < STRUGGLE_CADENCE_FRACTION * median);
            if cadence_down && power * 100 < target * STRUGGLE_POWER_PCT {
                collapsed += 1;
                if collapsed >= STRUGGLE_SECONDS {
                    return Some(Failure::CadenceCollapse);
                }
            } else {
                collapsed = 0;
            }
        }
        if power * 100 < target * SHORTFALL_POWER_PCT {
            short += 1;
            if short >= SHORTFALL_SECONDS {
                return Some(Failure::PowerShortfall);
            }
        } else {
            short = 0;
        }
    }
    None
}

/// Heart-rate drift across a long steady segment: second half over first half.
///
/// Only steady segments qualify — on a ramp, heart rate rising through the
/// second half says the target rose, not that the rider is decoupling.
fn segment_drift(run: &[DataPoint]) -> Option<f32> {
    if run.len() < DRIFT_MIN_SECONDS {
        return None;
    }
    let targets: Vec<u32> = run.iter().filter_map(|p| p.target_watts).collect();
    let mean_target = mean_of(targets.iter().copied());
    let spread = (*targets.iter().max()? - *targets.iter().min()?) as f32;
    if mean_target <= 0.0 || spread > STEADY_TARGET_TOLERANCE * mean_target {
        return None;
    }
    let half = run.len() / 2;
    let first = mean_hr(&run[..half])?;
    let second = mean_hr(&run[half..])?;
    if first <= 0.0 {
        return None;
    }
    Some(second / first)
}

fn mean_hr(run: &[DataPoint]) -> Option<f32> {
    let beats: Vec<u32> = run.iter().filter_map(|p| p.heart_rate_bpm).collect();
    if beats.is_empty() {
        return None;
    }
    Some(mean_of(beats.into_iter()))
}

// ── Window summary ──────────────────────────────────────────────────────────

/// Reduce every ride's evidence to the figures the rules read.
///
/// `evidence` may span any period; the window boundaries are applied here. The
/// 28 days before the analysis window supply the drift baseline only.
pub fn summarise(evidence: &[SessionEvidence], today: NaiveDate) -> WindowSummary {
    let age = |e: &SessionEvidence| (today - e.date).num_days();
    let current: Vec<&SessionEvidence> = evidence
        .iter()
        .filter(|e| (0..WINDOW_DAYS).contains(&age(e)) && e.is_hard_evidence())
        .collect();
    let previous: Vec<&SessionEvidence> = evidence
        .iter()
        .filter(|e| (WINDOW_DAYS..2 * WINDOW_DAYS).contains(&age(e)) && e.is_hard_evidence())
        .collect();

    let segments: Vec<&HardSegment> = current.iter().flat_map(|e| e.segments.iter()).collect();
    let failed = segments.iter().filter(|s| s.failed()).count();
    let total_secs: u32 = segments.iter().map(|s| s.duration_secs).sum();
    let avg_compliance = if total_secs == 0 {
        0.0
    } else {
        segments
            .iter()
            .map(|s| s.compliance * s.duration_secs as f32)
            .sum::<f32>()
            / total_secs as f32
    };

    let rpes: Vec<f32> = current
        .iter()
        .filter_map(|e| e.rpe)
        .map(f32::from)
        .collect();
    // One rider's word on one ride is an anecdote; the rules need two.
    let avg_rpe_hard = (rpes.len() >= 2).then(|| rpes.iter().sum::<f32>() / rpes.len() as f32);

    let mean_window_drift = |window: &[&SessionEvidence]| -> Option<f32> {
        let drifts: Vec<f32> = window.iter().filter_map(|e| e.mean_drift()).collect();
        (!drifts.is_empty()).then(|| drifts.iter().sum::<f32>() / drifts.len() as f32)
    };
    let drift_trend = match (mean_window_drift(&current), mean_window_drift(&previous)) {
        (Some(now), Some(before)) => Some((before - now) * 100.0),
        _ => None,
    };

    WindowSummary {
        n_hard: current.len(),
        total_segments: segments.len(),
        failed_segments: failed,
        fail_rate: if segments.is_empty() {
            0.0
        } else {
            failed as f32 / segments.len() as f32
        },
        avg_compliance,
        avg_rpe_hard,
        drift_trend,
        cadence_present: current.iter().any(|e| e.cadence_present),
    }
}

// ── Suggestion rules ────────────────────────────────────────────────────────

/// Decide what, if anything, to tell the rider about their FTP.
///
/// `last_change` is the date of the most recent `ftp_history` entry, whatever
/// set it; `icu_eftp` is the eFTP synced from Intervals.icu, which sees the
/// outdoor rides where real maximal efforts happen. Returns `None` when the
/// window holds too little threshold work to say anything at all — the check-in
/// card stays dark rather than showing a verdict drawn from one session.
pub fn suggest(
    summary: &WindowSummary,
    current_ftp: u32,
    last_change: Option<NaiveDate>,
    today: NaiveDate,
    icu_eftp: Option<u32>,
) -> Option<FtpSuggestion> {
    if summary.n_hard < MIN_HARD_SESSIONS {
        return None;
    }
    let days_since_change = last_change.map(|d| (today - d).num_days());
    let allowed = |cooldown: i64| days_since_change.is_none_or(|days| days >= cooldown);

    let rpe = summary.avg_rpe_hard;
    let wants_down = summary.fail_rate >= DOWN_FAIL_RATE
        || (rpe.is_some_and(|r| r >= DOWN_RPE) && summary.fail_rate >= DOWN_RPE_FAIL_RATE);

    if wants_down {
        if !allowed(DOWN_COOLDOWN_DAYS) {
            return Some(hold(
                current_ftp,
                cooldown_evidence(summary, days_since_change),
            ));
        }
        let magnitude = (2.0 + 10.0 * summary.fail_rate).clamp(2.0, MAX_DELTA_PCT);
        return Some(scaled(
            current_ftp,
            -magnitude,
            Direction::Down,
            down_evidence(summary),
        ));
    }

    let clean = summary.fail_rate == 0.0
        && summary.avg_compliance >= UP_COMPLIANCE
        && rpe.is_some_and(|r| r <= UP_RPE);
    let cross_check = rpe.is_some_and(|r| r <= CROSS_CHECK_RPE)
        && icu_eftp.is_some_and(|e| e as f32 >= CROSS_CHECK_MARGIN * current_ftp as f32);

    if clean || cross_check {
        // Half the failure evidence is missing without cadence, and it is the
        // half that fails first. Raising FTP on a window that cannot see a
        // rider come apart is the one mistake worth refusing outright.
        if !summary.cadence_present {
            return Some(hold(current_ftp, no_cadence_evidence(summary)));
        }
        if !allowed(UP_COOLDOWN_DAYS) {
            return Some(hold(
                current_ftp,
                cooldown_evidence(summary, days_since_change),
            ));
        }
        if cross_check {
            if let Some(eftp) = icu_eftp {
                let mut evidence = up_evidence(summary);
                evidence.push(format!(
                    "Intervals.icu estimates {eftp} W from your outdoor rides"
                ));
                return Some(to_target(current_ftp, eftp, evidence));
            }
        }
        let mut magnitude: f32 = 2.0;
        if rpe.is_some_and(|r| r <= UP_RPE_EASY) {
            magnitude += 1.0;
        }
        if summary
            .drift_trend
            .is_some_and(|t| t >= DRIFT_IMPROVEMENT_PP)
        {
            magnitude += 1.0;
        }
        return Some(scaled(
            current_ftp,
            magnitude.min(MAX_DELTA_PCT),
            Direction::Up,
            up_evidence(summary),
        ));
    }

    Some(hold(current_ftp, hold_evidence(summary)))
}

/// Apply a percentage change, then bring the result back inside the guard rails
/// and restate the delta as what was actually applied.
fn scaled(
    current_ftp: u32,
    delta_pct: f32,
    direction: Direction,
    evidence: Vec<String>,
) -> FtpSuggestion {
    let raw = (current_ftp as f32 * (1.0 + delta_pct / 100.0)).round();
    let new_ftp = (raw as u32).clamp(MIN_FTP_WATTS, MAX_FTP_WATTS);
    FtpSuggestion {
        new_ftp,
        delta_pct: applied_delta(current_ftp, new_ftp),
        direction,
        evidence,
    }
}

/// Move to a specific FTP, respecting the same caps.
///
/// Capped in watts rather than by re-deriving a percentage: `1.05_f32 * 200.0`
/// is 209.99999, and truncating that quietly loses a watt.
fn to_target(current_ftp: u32, target: u32, evidence: Vec<String>) -> FtpSuggestion {
    let step = (current_ftp as f32 * MAX_DELTA_PCT / 100.0).round() as u32;
    let new_ftp = target
        .clamp(current_ftp.saturating_sub(step), current_ftp + step)
        .clamp(MIN_FTP_WATTS, MAX_FTP_WATTS);
    FtpSuggestion {
        new_ftp,
        delta_pct: applied_delta(current_ftp, new_ftp),
        direction: match new_ftp.cmp(&current_ftp) {
            std::cmp::Ordering::Greater => Direction::Up,
            std::cmp::Ordering::Less => Direction::Down,
            std::cmp::Ordering::Equal => Direction::Hold,
        },
        evidence,
    }
}

fn applied_delta(current_ftp: u32, new_ftp: u32) -> f32 {
    if current_ftp == 0 {
        return 0.0;
    }
    (new_ftp as f32 - current_ftp as f32) / current_ftp as f32 * 100.0
}

fn hold(current_ftp: u32, evidence: Vec<String>) -> FtpSuggestion {
    FtpSuggestion {
        new_ftp: current_ftp,
        delta_pct: 0.0,
        direction: Direction::Hold,
        evidence,
    }
}

// ── Evidence sentences ──────────────────────────────────────────────────────

fn sessions_phrase(summary: &WindowSummary) -> String {
    let plural = if summary.n_hard == 1 { "" } else { "s" };
    format!("{} threshold session{plural}", summary.n_hard)
}

fn completion_line(summary: &WindowSummary) -> String {
    let completed = summary.total_segments - summary.failed_segments;
    format!(
        "{completed} of {} threshold intervals completed, averaging {:.0} % of target",
        summary.total_segments,
        summary.avg_compliance * 100.0
    )
}

fn rpe_line(summary: &WindowSummary) -> Option<String> {
    summary.avg_rpe_hard.map(|r| {
        format!(
            "Average RPE {r:.1} across {}",
            sessions_phrase(summary).to_lowercase()
        )
    })
}

fn drift_line(summary: &WindowSummary) -> Option<String> {
    let trend = summary.drift_trend?;
    if trend >= DRIFT_IMPROVEMENT_PP {
        Some(format!(
            "Heart-rate drift improved {trend:.0} points on the previous four weeks"
        ))
    } else if trend <= -DRIFT_IMPROVEMENT_PP {
        Some(format!(
            "Heart-rate drift worsened {:.0} points on the previous four weeks",
            -trend
        ))
    } else {
        None
    }
}

fn base_evidence(summary: &WindowSummary) -> Vec<String> {
    let mut lines = vec![completion_line(summary)];
    lines.extend(rpe_line(summary));
    lines.extend(drift_line(summary));
    lines
}

fn down_evidence(summary: &WindowSummary) -> Vec<String> {
    let mut lines = vec![format!(
        "{} of {} threshold intervals came apart",
        summary.failed_segments, summary.total_segments
    )];
    lines.extend(rpe_line(summary));
    lines.push("Easing FTP so the hard sessions land where they should".into());
    lines
}

fn up_evidence(summary: &WindowSummary) -> Vec<String> {
    base_evidence(summary)
}

fn hold_evidence(summary: &WindowSummary) -> Vec<String> {
    let mut lines = base_evidence(summary);
    lines.push("The evidence agrees with your current FTP".into());
    lines
}

fn no_cadence_evidence(summary: &WindowSummary) -> Vec<String> {
    let mut lines = base_evidence(summary);
    lines.push(
        "No cadence was recorded, so a session coming apart would go unseen — \
         holding until a cadence sensor is reporting"
            .into(),
    );
    lines
}

fn cooldown_evidence(summary: &WindowSummary, days_since_change: Option<i64>) -> Vec<String> {
    let mut lines = base_evidence(summary);
    if let Some(days) = days_since_change {
        lines.push(format!(
            "Your FTP changed {days} day{} ago — waiting for more evidence before moving it again",
            if days == 1 { "" } else { "s" }
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    const FTP: u32 = 200;
    /// 115 % of FTP — a 4×4 interval target, comfortably inside Z5.
    const HARD_TARGET: u32 = 230;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 6).expect("hardcoded valid date")
    }

    fn days_ago(n: i64) -> NaiveDate {
        today() - Duration::days(n)
    }

    /// One stretch of a synthetic ride held at a constant target.
    #[derive(Clone, Copy)]
    struct Block {
        secs: u32,
        target: u32,
        power: Option<u32>,
        cadence: Option<u32>,
        hr: Option<u32>,
    }

    /// A stretch ridden on target, with a cadence sensor reporting.
    fn block(secs: u32, target: u32, power: u32) -> Block {
        Block {
            secs,
            target,
            power: Some(power),
            cadence: Some(90),
            hr: Some(150),
        }
    }

    impl Block {
        fn cadence(mut self, rpm: Option<u32>) -> Self {
            self.cadence = rpm;
            self
        }
        fn hr(mut self, bpm: u32) -> Self {
            self.hr = Some(bpm);
            self
        }
        fn no_power(mut self) -> Self {
            self.power = None;
            self
        }
    }

    /// A recovery valley: well below threshold, so it separates hard runs.
    fn easy(secs: u32) -> Block {
        block(secs, 100, 100)
    }

    /// Four minutes on target — one interval of a 4×4 session.
    fn hard(secs: u32) -> Block {
        block(secs, HARD_TARGET, 228)
    }

    fn ride(date: NaiveDate, rpe: Option<u8>, blocks: &[Block]) -> Session {
        let started_at = Local
            .from_local_datetime(&date.and_hms_opt(9, 0, 0).expect("hardcoded valid time"))
            .earliest()
            .expect("9am exists on every day of this test suite")
            .with_timezone(&Utc);
        let mut points = Vec::new();
        for b in blocks {
            for _ in 0..b.secs {
                points.push(DataPoint {
                    elapsed_secs: points.len() as u32,
                    power_watts: b.power,
                    target_watts: Some(b.target),
                    heart_rate_bpm: b.hr,
                    cadence_rpm: b.cadence,
                    speed_kmh: None,
                    lat: None,
                    lng: None,
                    altitude_m: None,
                });
            }
        }
        Session {
            id: 0,
            workout_id: None,
            started_at,
            ended_at: Some(started_at + Duration::seconds(points.len() as i64)),
            data_points: points,
            rpe,
            ftp_watts: Some(FTP),
            title: None,
            icu_id: None,
        }
    }

    /// A clean 4×4: 16 minutes of hard work, every interval held.
    fn clean_session(date: NaiveDate, rpe: u8) -> SessionEvidence {
        let s = ride(
            date,
            Some(rpe),
            &[
                easy(600),
                hard(240),
                easy(240),
                hard(240),
                easy(240),
                hard(240),
                easy(240),
                hard(240),
            ],
        );
        session_evidence(&s, None).expect("targets and a stamped FTP are present")
    }

    /// A single ten-minute threshold block that fell apart part-way through.
    fn failed_session(date: NaiveDate, rpe: u8) -> SessionEvidence {
        let s = ride(
            date,
            Some(rpe),
            &[
                easy(600),
                hard(300),
                block(300, HARD_TARGET, 190), // 83 % of target for five minutes
            ],
        );
        session_evidence(&s, None).expect("targets and a stamped FTP are present")
    }

    /// A single ten-minute threshold block, held.
    fn held_session(date: NaiveDate, rpe: u8) -> SessionEvidence {
        let s = ride(date, Some(rpe), &[easy(600), hard(600)]);
        session_evidence(&s, None).expect("targets and a stamped FTP are present")
    }

    // ── Evidence extraction ─────────────────────────────────────────────────

    #[test]
    fn should_ignore_a_ride_recorded_before_ftp_stamping() {
        let mut s = clean_ride();
        s.ftp_watts = None;
        assert!(session_evidence(&s, None).is_none());
    }

    #[test]
    fn should_ignore_a_route_ride_that_recorded_no_targets() {
        let mut s = clean_ride();
        for p in &mut s.data_points {
            p.target_watts = None;
        }
        assert!(session_evidence(&s, None).is_none());
    }

    fn clean_ride() -> Session {
        ride(today(), Some(5), &[easy(600), hard(600)])
    }

    #[test]
    fn should_count_a_target_at_exactly_ninety_one_percent_as_hard() {
        // 91 % of 200 W is 182 W exactly. In f32 the same comparison is
        // 182 >= 182.000005, which drops the interval that defines the boundary.
        let s = ride(today(), Some(5), &[block(600, 182, 182)]);
        let e = session_evidence(&s, None).expect("evidence");
        assert_eq!(e.hard_seconds(), 600);
    }

    #[test]
    fn should_not_count_a_sweet_spot_target_at_ninety_percent_as_hard() {
        let s = ride(today(), Some(5), &[block(600, 180, 180)]);
        let e = session_evidence(&s, None).expect("evidence");
        assert_eq!(e.hard_seconds(), 0);
        assert!(!e.is_hard_evidence());
    }

    #[test]
    fn should_need_ten_minutes_of_hard_work_to_be_evidence() {
        let short = ride(today(), Some(5), &[hard(599)]);
        let long = ride(today(), Some(5), &[hard(600)]);
        assert!(!session_evidence(&short, None)
            .expect("evidence")
            .is_hard_evidence());
        assert!(session_evidence(&long, None)
            .expect("evidence")
            .is_hard_evidence());
    }

    #[test]
    fn should_split_hard_work_into_one_segment_per_interval() {
        let e = clean_session(today(), 5);
        assert_eq!(e.segments.len(), 4);
        assert_eq!(e.hard_seconds(), 960);
        assert!(e.segments.iter().all(|s| !s.failed()));
        assert!((e.segments[0].compliance - 228.0 / 230.0).abs() < 0.001);
    }

    #[test]
    fn should_fail_a_segment_when_cadence_collapses_under_target() {
        // Power is above the 90 % shortfall line, so only the cadence rule can
        // fire — which is what makes this the spiral of death and not a wobble.
        let s = ride(
            today(),
            Some(8),
            &[
                easy(600),
                hard(300),
                block(30, HARD_TARGET, 210).cadence(Some(60)),
                hard(270),
            ],
        );
        let e = session_evidence(&s, None).expect("evidence");
        assert_eq!(e.segments[0].failure, Some(Failure::CadenceCollapse));
    }

    #[test]
    fn should_fail_a_segment_on_power_alone_when_no_cadence_is_recorded() {
        // Every session recorded so far reports a flat zero cadence. Without
        // this rule nothing in such a window could ever be judged a failure.
        let blocks: Vec<Block> = [easy(600), hard(300), block(300, HARD_TARGET, 190)]
            .iter()
            .map(|b| b.cadence(Some(0)))
            .collect();
        let s = ride(today(), Some(8), &blocks);
        let e = session_evidence(&s, None).expect("evidence");
        assert!(!e.cadence_present);
        assert_eq!(e.segments[0].failure, Some(Failure::PowerShortfall));
    }

    #[test]
    fn should_not_fail_a_segment_when_the_shortfall_is_too_brief() {
        let s = ride(
            today(),
            Some(5),
            &[easy(600), hard(300), block(29, HARD_TARGET, 190), hard(271)],
        );
        let e = session_evidence(&s, None).expect("evidence");
        assert_eq!(e.segments[0].failure, None);
    }

    #[test]
    fn should_not_fail_a_segment_when_the_power_meter_dropped_out() {
        // Missing seconds are missing evidence, not evidence of failure.
        let s = ride(
            today(),
            Some(5),
            &[easy(600), hard(300), hard(120).no_power(), hard(180)],
        );
        let e = session_evidence(&s, None).expect("evidence");
        assert_eq!(e.segments[0].failure, None);
    }

    #[test]
    fn should_read_a_flat_zero_cadence_as_no_sensor_at_all() {
        let with_sensor = ride(today(), Some(5), &[hard(600)]);
        let without = ride(today(), Some(5), &[hard(600).cadence(Some(0))]);
        let absent = ride(today(), Some(5), &[hard(600).cadence(None)]);
        assert!(
            session_evidence(&with_sensor, None)
                .expect("evidence")
                .cadence_present
        );
        assert!(
            !session_evidence(&without, None)
                .expect("evidence")
                .cadence_present
        );
        assert!(
            !session_evidence(&absent, None)
                .expect("evidence")
                .cadence_present
        );
    }

    #[test]
    fn should_fail_the_last_segment_when_the_ride_was_abandoned_in_it() {
        let s = ride(today(), Some(9), &[easy(600), hard(600)]);
        let e = session_evidence(&s, Some(3600)).expect("evidence");
        assert_eq!(e.segments[0].failure, Some(Failure::StoppedEarly));
    }

    #[test]
    fn should_not_fail_a_segment_when_the_ride_ran_nearly_to_plan() {
        let s = ride(today(), Some(5), &[easy(600), hard(600)]);
        let e = session_evidence(&s, Some(1250)).expect("evidence");
        assert_eq!(e.segments[0].failure, None);
    }

    #[test]
    fn should_read_drift_from_a_long_steady_segment_only() {
        let long = ride(today(), Some(5), &[hard(300).hr(150), hard(300).hr(153)]);
        let e = session_evidence(&long, None).expect("evidence");
        let drift = e.segments[0]
            .drift
            .expect("a ten-minute steady block has drift");
        assert!((drift - 1.02).abs() < 0.001, "drift was {drift}");

        let short = ride(today(), Some(5), &[hard(200).hr(150), hard(200).hr(160)]);
        let e = session_evidence(&short, None).expect("evidence");
        assert_eq!(e.segments[0].drift, None);
    }

    // ── Window summary ──────────────────────────────────────────────────────

    #[test]
    fn should_ignore_sessions_older_than_the_analysis_window() {
        let evidence = vec![
            clean_session(days_ago(1), 5),
            clean_session(days_ago(27), 5),
            clean_session(days_ago(28), 5),
            clean_session(days_ago(40), 5),
        ];
        let summary = summarise(&evidence, today());
        assert_eq!(summary.n_hard, 2);
        assert_eq!(summary.total_segments, 8);
    }

    #[test]
    fn should_ignore_sessions_with_too_little_hard_work() {
        let light =
            session_evidence(&ride(today(), Some(5), &[hard(300)]), None).expect("evidence");
        let summary = summarise(&[light], today());
        assert_eq!(summary.n_hard, 0);
        assert_eq!(summary.total_segments, 0);
    }

    #[test]
    fn should_need_two_rpe_values_before_averaging_them() {
        let one = vec![clean_session(days_ago(1), 5)];
        assert_eq!(summarise(&one, today()).avg_rpe_hard, None);

        let two = vec![clean_session(days_ago(1), 5), clean_session(days_ago(3), 7)];
        assert_eq!(summarise(&two, today()).avg_rpe_hard, Some(6.0));
    }

    #[test]
    fn should_weight_compliance_by_how_long_each_interval_lasted() {
        let s = ride(
            today(),
            Some(5),
            &[
                block(900, HARD_TARGET, 230), // 15 min exactly on target
                easy(60),
                block(300, HARD_TARGET, 184), // 5 min at 80 %
            ],
        );
        let e = session_evidence(&s, None).expect("evidence");
        let summary = summarise(&[e], today());
        // 0.75 × 1.0 + 0.25 × 0.8 — not the unweighted 0.9.
        assert!(
            (summary.avg_compliance - 0.95).abs() < 0.01,
            "compliance was {}",
            summary.avg_compliance
        );
    }

    #[test]
    fn should_report_drift_improvement_against_the_previous_window() {
        let steady = |date: NaiveDate, first: u32, second: u32| {
            session_evidence(
                &ride(date, Some(5), &[hard(300).hr(first), hard(300).hr(second)]),
                None,
            )
            .expect("evidence")
        };
        let evidence = vec![
            steady(days_ago(2), 150, 153),  // drift 1.02
            steady(days_ago(35), 150, 159), // drift 1.06
        ];
        let trend = summarise(&evidence, today())
            .drift_trend
            .expect("both windows have drift");
        assert!((trend - 4.0).abs() < 0.1, "trend was {trend}");
    }

    // ── Suggestion rules ────────────────────────────────────────────────────

    fn summary_of(evidence: &[SessionEvidence]) -> WindowSummary {
        summarise(evidence, today())
    }

    #[test]
    fn should_make_no_suggestion_with_fewer_than_three_hard_sessions() {
        let evidence = vec![clean_session(days_ago(1), 4), clean_session(days_ago(4), 4)];
        assert_eq!(
            suggest(&summary_of(&evidence), FTP, None, today(), None),
            None
        );
    }

    #[test]
    fn should_suggest_a_rise_when_every_interval_was_held_at_low_rpe() {
        let evidence = vec![
            clean_session(days_ago(2), 5),
            clean_session(days_ago(5), 5),
            clean_session(days_ago(9), 5),
        ];
        let s = suggest(&summary_of(&evidence), FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Up);
        assert_eq!(s.new_ftp, 206); // +3 %: 2 % base, +1 % for RPE 5
        assert!((s.delta_pct - 3.0).abs() < 0.01);
        assert!(s.evidence.iter().any(|l| l.contains("RPE 5.0")));
    }

    #[test]
    fn should_withhold_a_rise_when_no_cadence_was_recorded_all_window() {
        let blind = |date: NaiveDate| {
            let blocks: Vec<Block> = [easy(600), hard(600)]
                .iter()
                .map(|b| b.cadence(Some(0)))
                .collect();
            session_evidence(&ride(date, Some(5), &blocks), None).expect("evidence")
        };
        let evidence = vec![blind(days_ago(2)), blind(days_ago(5)), blind(days_ago(9))];
        let summary = summary_of(&evidence);
        assert!(!summary.cadence_present);

        let s = suggest(&summary, FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Hold);
        assert_eq!(s.new_ftp, FTP);
        assert!(s.evidence.iter().any(|l| l.contains("No cadence")));
    }

    #[test]
    fn should_still_suggest_a_drop_when_no_cadence_was_recorded() {
        // Being half blind is a reason not to raise FTP, never a reason to
        // leave it too high once a session has visibly come apart.
        let blind = |date: NaiveDate, rpe: u8, powers: (u32, u32)| {
            let blocks: Vec<Block> = [
                easy(600),
                block(300, HARD_TARGET, powers.0),
                block(300, HARD_TARGET, powers.1),
            ]
            .iter()
            .map(|b| b.cadence(Some(0)))
            .collect();
            session_evidence(&ride(date, Some(rpe), &blocks), None).expect("evidence")
        };
        let evidence = vec![
            blind(days_ago(2), 9, (228, 190)),
            blind(days_ago(5), 9, (228, 190)),
            blind(days_ago(9), 8, (228, 228)),
        ];
        let s = suggest(&summary_of(&evidence), FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Down);
    }

    #[test]
    fn should_suggest_a_drop_when_a_quarter_of_intervals_come_apart() {
        let evidence = vec![
            failed_session(days_ago(2), 8),
            held_session(days_ago(5), 7),
            held_session(days_ago(9), 7),
            held_session(days_ago(12), 7),
        ];
        let summary = summary_of(&evidence);
        assert!((summary.fail_rate - 0.25).abs() < 0.001);

        let s = suggest(&summary, FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Down);
        assert_eq!(s.new_ftp, 191); // −4.5 %: 2 + 10 × 0.25
    }

    #[test]
    fn should_cap_a_drop_at_five_percent_however_bad_the_window() {
        let evidence = vec![
            failed_session(days_ago(2), 9),
            failed_session(days_ago(5), 9),
            failed_session(days_ago(9), 9),
        ];
        let s = suggest(&summary_of(&evidence), FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Down);
        assert!(
            (s.delta_pct + 5.0).abs() < 0.01,
            "delta was {}",
            s.delta_pct
        );
        assert_eq!(s.new_ftp, 190);
    }

    #[test]
    fn should_ease_on_high_rpe_even_when_most_intervals_were_held() {
        let evidence = vec![
            failed_session(days_ago(2), 9),
            held_session(days_ago(5), 9),
            held_session(days_ago(9), 9),
            held_session(days_ago(12), 9),
            held_session(days_ago(15), 9),
        ];
        let summary = summary_of(&evidence);
        assert!(summary.fail_rate < DOWN_FAIL_RATE);
        let s = suggest(&summary, FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Down);
    }

    #[test]
    fn should_hold_a_rise_inside_the_twenty_one_day_cooldown() {
        let evidence = vec![
            clean_session(days_ago(2), 5),
            clean_session(days_ago(5), 5),
            clean_session(days_ago(9), 5),
        ];
        let s = suggest(
            &summary_of(&evidence),
            FTP,
            Some(days_ago(10)),
            today(),
            None,
        )
        .expect("a suggestion");
        assert_eq!(s.direction, Direction::Hold);
        assert!(s.evidence.iter().any(|l| l.contains("10 days ago")));

        let after = suggest(
            &summary_of(&evidence),
            FTP,
            Some(days_ago(21)),
            today(),
            None,
        )
        .expect("a suggestion");
        assert_eq!(after.direction, Direction::Up);
    }

    #[test]
    fn should_allow_a_drop_after_only_seven_days() {
        let evidence = vec![
            failed_session(days_ago(2), 9),
            failed_session(days_ago(5), 9),
            failed_session(days_ago(9), 9),
        ];
        let held = suggest(
            &summary_of(&evidence),
            FTP,
            Some(days_ago(3)),
            today(),
            None,
        )
        .expect("a suggestion");
        assert_eq!(held.direction, Direction::Hold);

        let eased = suggest(
            &summary_of(&evidence),
            FTP,
            Some(days_ago(7)),
            today(),
            None,
        )
        .expect("a suggestion");
        assert_eq!(eased.direction, Direction::Down);
    }

    #[test]
    fn should_trust_an_intervals_eftp_only_as_far_as_five_percent() {
        // Compliance is short of the clean-window bar, so only the cross-check
        // can raise FTP here.
        let almost = |date: NaiveDate| {
            session_evidence(
                &ride(date, Some(6), &[easy(600), block(600, HARD_TARGET, 220)]),
                None,
            )
            .expect("evidence")
        };
        let evidence = vec![
            almost(days_ago(2)),
            almost(days_ago(5)),
            almost(days_ago(9)),
        ];
        let summary = summary_of(&evidence);
        assert!(summary.avg_compliance < UP_COMPLIANCE);

        let s = suggest(&summary, FTP, None, today(), Some(260)).expect("a suggestion");
        assert_eq!(s.direction, Direction::Up);
        assert_eq!(s.new_ftp, 210); // capped at 1.05 × 200, not 260
        assert!(s.evidence.iter().any(|l| l.contains("Intervals.icu")));
    }

    #[test]
    fn should_ignore_an_intervals_eftp_that_barely_differs() {
        let almost = |date: NaiveDate| {
            session_evidence(
                &ride(date, Some(6), &[easy(600), block(600, HARD_TARGET, 220)]),
                None,
            )
            .expect("evidence")
        };
        let evidence = vec![
            almost(days_ago(2)),
            almost(days_ago(5)),
            almost(days_ago(9)),
        ];
        let s =
            suggest(&summary_of(&evidence), FTP, None, today(), Some(202)).expect("a suggestion");
        assert_eq!(s.direction, Direction::Hold);
    }

    #[test]
    fn should_hold_when_the_window_says_the_number_is_right() {
        let evidence = vec![
            held_session(days_ago(2), 7),
            held_session(days_ago(5), 7),
            held_session(days_ago(9), 7),
        ];
        let s = suggest(&summary_of(&evidence), FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Hold);
        assert_eq!(s.new_ftp, FTP);
        assert!(s.evidence.iter().any(|l| l.contains("agrees")));
    }

    #[test]
    fn should_never_suggest_an_ftp_outside_the_plausible_range() {
        let up = vec![
            clean_session(days_ago(2), 5),
            clean_session(days_ago(5), 5),
            clean_session(days_ago(9), 5),
        ];
        let s =
            suggest(&summary_of(&up), MAX_FTP_WATTS, None, today(), None).expect("a suggestion");
        assert_eq!(s.new_ftp, MAX_FTP_WATTS);

        let down = vec![
            failed_session(days_ago(2), 9),
            failed_session(days_ago(5), 9),
            failed_session(days_ago(9), 9),
        ];
        let s =
            suggest(&summary_of(&down), MIN_FTP_WATTS, None, today(), None).expect("a suggestion");
        assert_eq!(s.new_ftp, MIN_FTP_WATTS);
    }

    #[test]
    fn should_add_a_point_of_confidence_when_drift_improved() {
        let steady = |date: NaiveDate, first: u32, second: u32| {
            session_evidence(
                &ride(date, Some(5), &[hard(300).hr(first), hard(300).hr(second)]),
                None,
            )
            .expect("evidence")
        };
        let evidence = vec![
            steady(days_ago(2), 150, 153),
            steady(days_ago(5), 150, 153),
            steady(days_ago(9), 150, 153),
            steady(days_ago(33), 150, 159),
            steady(days_ago(36), 150, 159),
        ];
        let s = suggest(&summary_of(&evidence), FTP, None, today(), None).expect("a suggestion");
        assert_eq!(s.direction, Direction::Up);
        assert_eq!(s.new_ftp, 208); // 2 % base + 1 % for RPE 5 + 1 % for drift
        assert!(s.evidence.iter().any(|l| l.contains("drift improved")));
    }
}
