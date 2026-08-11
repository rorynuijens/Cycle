//! What a coach would say at the start of each interval.
//!
//! The cockpit already reports what is happening — target watts, seconds left,
//! which zone the effort sits in — but never what it is *for*. Mid-effort the
//! facts a rider actually wants are not numbers: which rep this is out of how
//! many, how many are still to come after it, and whether this is the one to
//! hold on to. The player's "Interval N" counts segments, so in a workout with
//! recoveries between efforts it calls the second rep "Interval 4" — which is
//! worse than saying nothing.
//!
//! Everything here is derived from the workout itself and from the rider's
//! earlier attempts at it, so cues cost nothing per ride and appear whether or
//! not an API key is set.
//!
//! Deliberately free of watt figures. The hero row shows the target the trainer
//! was actually given, intensity dial included; a number baked in here at the
//! start of the ride would contradict it the moment the rider dials the session
//! down.

use crate::data::workout::{Segment, Workout};
use crate::training::engine::WorkoutEngine;
use crate::training::progression::Effort;

/// At or below this share of FTP a segment is a rest, not an effort.
///
/// 60 % is the top of zone 1 in the same Coggan scale the rest of the app uses,
/// so anything a rider would describe as "spinning" falls below it.
const RECOVERY_CEILING_PCT: f32 = 60.0;

/// A work interval shorter than this ends before a closing nudge could help —
/// on a 45-second effort the cue would cover most of the interval.
const MIN_CLOSING_INTERVAL_SECS: u32 = 90;

/// How long before the end of a work interval the closing cue takes over.
pub const CLOSING_SECS: u32 = 30;

/// What a segment is for, inferred from its power and its place in the workout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRole {
    WarmUp,
    Work,
    Recovery,
    CoolDown,
}

impl SegmentRole {
    /// True for the segments the rider is working through rather than resting in.
    /// The player leans on this to emphasise a cue that matters.
    pub fn is_effort(&self) -> bool {
        matches!(self, SegmentRole::Work)
    }
}

/// One segment's worth of coaching.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentCue {
    pub role: SegmentRole,
    /// Shown as soon as the segment begins, and for its whole duration.
    pub headline: String,
    /// A quieter second line — how to ride it, or what came before.
    pub detail: Option<String>,
    /// Replaces `detail` for the closing [`CLOSING_SECS`] of a work interval.
    /// `None` wherever counting down would not help.
    pub closing: Option<String>,
}

/// Build one cue per segment, indexable by `EngineSnapshot::segment_index`.
///
/// `priors` are the rider's earlier attempts at this same workout, already
/// filtered by name — see [`crate::training::progression::prior_efforts`]. An
/// empty slice simply drops the history line; it is not an error.
pub fn build_cues(workout: &Workout, priors: &[Effort]) -> Vec<SegmentCue> {
    let segments = &workout.segments;
    let roles = roles_of(segments);
    let work_total = roles.iter().filter(|r| r.is_effort()).count();
    let history = history_line(priors);

    let mut work_seen = 0usize;
    let mut cues = Vec::with_capacity(segments.len());

    for (i, (segment, role)) in segments.iter().zip(&roles).enumerate() {
        if role.is_effort() {
            work_seen += 1;
        }
        let is_final_rep = role.is_effort() && work_seen == work_total;

        let headline = match role {
            SegmentRole::WarmUp => "Warm-up".to_string(),
            SegmentRole::Recovery => "Recovery".to_string(),
            SegmentRole::CoolDown => "Cool-down".to_string(),
            SegmentRole::Work if work_total > 1 => {
                format!("Rep {work_seen} of {work_total}")
            }
            // A single effort has no rep to count, so its own name is the most
            // informative thing available.
            SegmentRole::Work => segment
                .label
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .unwrap_or("The main effort")
                .to_string(),
        };

        let detail = match role {
            // The history belongs on the first segment, where there is time to
            // read it — not repeated over every rep.
            SegmentRole::WarmUp if i == 0 => history
                .clone()
                .or_else(|| secs_until_work(segments, &roles, i).map(warm_up_detail)),
            SegmentRole::WarmUp => secs_until_work(segments, &roles, i).map(warm_up_detail),
            SegmentRole::Recovery => Some(reps_to_go(work_total - work_seen)),
            SegmentRole::CoolDown => Some("Session done. Spin it out.".to_string()),
            SegmentRole::Work => work_detail(segment, is_final_rep, work_total),
        };

        let closing = if role.is_effort() && segment.duration_secs >= MIN_CLOSING_INTERVAL_SECS {
            Some(if is_final_rep && work_total > 1 {
                format!("Last {CLOSING_SECS} seconds of the last rep — finish it.")
            } else {
                format!("Last {CLOSING_SECS} seconds — hold your form.")
            })
        } else {
            None
        };

        cues.push(SegmentCue {
            role: *role,
            headline,
            detail,
            closing,
        });
    }

    cues
}

/// A segment's power as a single figure — the mid-point, so a ramp is judged on
/// its average rather than on whichever end happens to be written first.
fn mid_pct(segment: &Segment) -> f32 {
    (segment.power_low_pct + segment.power_high_pct) / 2.0
}

/// Assign a role to every segment.
///
/// Position alone is not enough: the leading easy segment of an interval session
/// is a warm-up, but the leading easy segment of a recovery spin is just the
/// ride. So an easy segment is a warm-up only while no effort has happened yet,
/// a cool-down only once none remain, and a recovery in between.
fn roles_of(segments: &[Segment]) -> Vec<SegmentRole> {
    let is_work: Vec<bool> = segments
        .iter()
        .map(|s| mid_pct(s) > RECOVERY_CEILING_PCT)
        .collect();

    // No effort anywhere — a recovery ride. Calling its first half a warm-up for
    // work that never arrives would be a lie.
    if !is_work.iter().any(|w| *w) {
        return vec![SegmentRole::Recovery; segments.len()];
    }

    (0..segments.len())
        .map(|i| {
            if is_work[i] {
                SegmentRole::Work
            } else if !is_work[..i].iter().any(|w| *w) {
                SegmentRole::WarmUp
            } else if !is_work[i + 1..].iter().any(|w| *w) {
                SegmentRole::CoolDown
            } else {
                SegmentRole::Recovery
            }
        })
        .collect()
}

/// Seconds from the start of segment `i` to the start of the first effort.
fn secs_until_work(segments: &[Segment], roles: &[SegmentRole], i: usize) -> Option<u32> {
    let first_work = roles.iter().position(|r| r.is_effort())?;
    if first_work <= i {
        return None;
    }
    Some(
        segments[i..first_work]
            .iter()
            .map(|s| s.duration_secs)
            .sum(),
    )
}

fn warm_up_detail(secs: u32) -> String {
    format!(
        "Ease in. First effort in {}.",
        WorkoutEngine::format_duration(secs)
    )
}

fn reps_to_go(remaining: usize) -> String {
    match remaining {
        0 => "Work done.".to_string(),
        1 => "One rep to go.".to_string(),
        n => format!("{n} reps to go."),
    }
}

/// How to ride this effort. Ordered by how actionable it is: an explicit cadence
/// target beats generic pacing advice, and both beat nothing.
fn work_detail(segment: &Segment, is_final_rep: bool, work_total: usize) -> Option<String> {
    if is_final_rep && work_total > 1 {
        return Some("Last one — make it count.".to_string());
    }
    if let Some(rpm) = segment.cadence_target {
        return Some(format!("Hold {rpm} rpm."));
    }
    if segment.is_ramp() {
        return Some("Build steadily — don't go out too hard.".to_string());
    }
    None
}

/// What the rider has done with this workout before.
///
/// Reports normalised power because that is what [`Effort::power`] prefers, and
/// a "best" that silently mixed normalised and average figures across attempts
/// would not be comparable.
fn history_line(priors: &[Effort]) -> Option<String> {
    if priors.is_empty() {
        return None;
    }
    let times = match priors.len() {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        n => format!("{n} times"),
    };
    match priors.iter().filter_map(Effort::power).max() {
        Some(best) => Some(format!(
            "You've ridden this {times} — best {best} W normalised."
        )),
        None => Some(format!("You've ridden this {times}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::WorkoutCategory;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    fn effort(day: u32, power: Option<u32>) -> Effort {
        Effort {
            date: date(2026, 8, day),
            name: "Cruise Intervals".into(),
            duration_secs: 3600,
            normalised_power: power,
            average_power: None,
            average_hr: None,
            distance_m: None,
            rpe: None,
        }
    }

    fn workout(segments: Vec<Segment>) -> Workout {
        Workout::from_segments("Test", "", WorkoutCategory::Threshold, segments)
    }

    /// Warm-up, three efforts with recoveries between them, cool-down.
    fn intervals() -> Workout {
        workout(vec![
            Segment::steady(600, 50.0, "Warm-up"),
            Segment::steady(480, 98.0, "Interval 1"),
            Segment::steady(300, 50.0, "Recovery"),
            Segment::steady(480, 98.0, "Interval 2"),
            Segment::steady(300, 50.0, "Recovery"),
            Segment::steady(480, 98.0, "Interval 3"),
            Segment::steady(600, 40.0, "Cool-down"),
        ])
    }

    // ── Shape ────────────────────────────────────────────────────────────────

    #[test]
    fn should_return_one_cue_per_segment() {
        let w = intervals();
        assert_eq!(build_cues(&w, &[]).len(), w.segments.len());
    }

    #[test]
    fn should_return_no_cues_when_workout_has_no_segments() {
        assert!(build_cues(&workout(vec![]), &[]).is_empty());
    }

    // ── Roles ────────────────────────────────────────────────────────────────

    #[test]
    fn should_assign_roles_by_power_and_position() {
        let cues = build_cues(&intervals(), &[]);
        let roles: Vec<SegmentRole> = cues.iter().map(|c| c.role).collect();
        assert_eq!(
            roles,
            vec![
                SegmentRole::WarmUp,
                SegmentRole::Work,
                SegmentRole::Recovery,
                SegmentRole::Work,
                SegmentRole::Recovery,
                SegmentRole::Work,
                SegmentRole::CoolDown,
            ]
        );
    }

    #[test]
    fn should_treat_60_percent_ftp_as_recovery() {
        let w = workout(vec![
            Segment::steady(600, 60.0, "Spin"),
            Segment::steady(600, 95.0, "Effort"),
        ]);
        assert_eq!(build_cues(&w, &[])[0].role, SegmentRole::WarmUp);
    }

    #[test]
    fn should_treat_61_percent_ftp_as_work() {
        let w = workout(vec![
            Segment::steady(600, 61.0, "Endurance"),
            Segment::steady(600, 95.0, "Effort"),
        ]);
        assert_eq!(build_cues(&w, &[])[0].role, SegmentRole::Work);
    }

    #[test]
    fn should_call_every_segment_recovery_when_nothing_is_hard() {
        let w = workout(vec![
            Segment::steady(600, 45.0, "Spin"),
            Segment::steady(1200, 55.0, "Spin"),
            Segment::steady(600, 40.0, "Spin"),
        ]);
        let cues = build_cues(&w, &[]);
        assert!(cues.iter().all(|c| c.role == SegmentRole::Recovery));
    }

    #[test]
    fn should_judge_a_ramp_on_its_midpoint() {
        // 40 → 90 averages 65 %, which is work despite starting easy.
        let w = workout(vec![
            Segment::ramp(600, 40.0, 90.0, "Ramp"),
            Segment::steady(600, 95.0, "Effort"),
        ]);
        assert_eq!(build_cues(&w, &[])[0].role, SegmentRole::Work);
    }

    #[test]
    fn should_not_call_a_trailing_easy_segment_a_cooldown_when_work_remains() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(cues[2].role, SegmentRole::Recovery);
        assert_eq!(cues[4].role, SegmentRole::Recovery);
    }

    // ── Rep numbering ────────────────────────────────────────────────────────

    #[test]
    fn should_number_reps_by_effort_not_by_segment() {
        let cues = build_cues(&intervals(), &[]);
        // Segment 3 is the *second* effort — the cockpit would call it Interval 4.
        assert_eq!(cues[1].headline, "Rep 1 of 3");
        assert_eq!(cues[3].headline, "Rep 2 of 3");
        assert_eq!(cues[5].headline, "Rep 3 of 3");
    }

    #[test]
    fn should_name_a_lone_effort_instead_of_numbering_it() {
        let w = workout(vec![
            Segment::steady(600, 50.0, "Warm-up"),
            Segment::steady(1200, 95.0, "Threshold block"),
            Segment::steady(300, 40.0, "Cool-down"),
        ]);
        assert_eq!(build_cues(&w, &[])[1].headline, "Threshold block");
    }

    #[test]
    fn should_fall_back_to_a_generic_name_for_an_unlabelled_lone_effort() {
        let w = workout(vec![
            Segment {
                duration_secs: 1200,
                power_low_pct: 95.0,
                power_high_pct: 95.0,
                label: Some("   ".into()),
                cadence_target: None,
            },
            Segment::steady(300, 40.0, "Cool-down"),
        ]);
        assert_eq!(build_cues(&w, &[])[0].headline, "The main effort");
    }

    // ── Recovery details ─────────────────────────────────────────────────────

    #[test]
    fn should_count_down_the_reps_that_remain() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(cues[2].detail.as_deref(), Some("2 reps to go."));
        assert_eq!(cues[4].detail.as_deref(), Some("One rep to go."));
    }

    #[test]
    fn should_tell_the_rider_the_session_is_done_on_the_cooldown() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(
            cues[6].detail.as_deref(),
            Some("Session done. Spin it out.")
        );
    }

    // ── Warm-up details ──────────────────────────────────────────────────────

    #[test]
    fn should_say_how_long_until_the_first_effort() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("Ease in. First effort in 10:00.")
        );
    }

    #[test]
    fn should_count_to_the_first_effort_across_several_warmup_segments() {
        let w = workout(vec![
            Segment::steady(300, 45.0, "Spin"),
            Segment::steady(300, 55.0, "Build"),
            Segment::steady(480, 98.0, "Interval 1"),
        ]);
        let cues = build_cues(&w, &[]);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("Ease in. First effort in 10:00.")
        );
        // From the second warm-up segment there are only five minutes left.
        assert_eq!(
            cues[1].detail.as_deref(),
            Some("Ease in. First effort in 5:00.")
        );
    }

    // ── History ──────────────────────────────────────────────────────────────

    #[test]
    fn should_put_the_history_on_the_first_segment_only() {
        let priors = vec![effort(1, Some(240)), effort(8, Some(268))];
        let cues = build_cues(&intervals(), &priors);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("You've ridden this twice — best 268 W normalised.")
        );
        // Later segments keep their own advice rather than repeating it.
        assert_eq!(cues[2].detail.as_deref(), Some("2 reps to go."));
    }

    #[test]
    fn should_report_a_single_prior_attempt_as_once() {
        let cues = build_cues(&intervals(), &[effort(1, Some(240))]);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("You've ridden this once — best 240 W normalised.")
        );
    }

    #[test]
    fn should_count_attempts_beyond_two_numerically() {
        let priors = vec![effort(1, Some(240)), effort(4, Some(250)), effort(8, None)];
        let cues = build_cues(&intervals(), &priors);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("You've ridden this 3 times — best 250 W normalised.")
        );
    }

    #[test]
    fn should_omit_the_best_when_no_prior_attempt_recorded_power() {
        let cues = build_cues(&intervals(), &[effort(1, None), effort(8, None)]);
        assert_eq!(cues[0].detail.as_deref(), Some("You've ridden this twice."));
    }

    #[test]
    fn should_fall_back_to_the_warmup_advice_when_there_is_no_history() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(
            cues[0].detail.as_deref(),
            Some("Ease in. First effort in 10:00.")
        );
    }

    // ── Work details ─────────────────────────────────────────────────────────

    #[test]
    fn should_flag_the_final_rep() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(cues[5].detail.as_deref(), Some("Last one — make it count."));
    }

    #[test]
    fn should_prefer_a_cadence_target_over_ramp_advice() {
        let w = workout(vec![
            Segment {
                duration_secs: 480,
                power_low_pct: 90.0,
                power_high_pct: 100.0,
                label: Some("Interval 1".into()),
                cadence_target: Some(95),
            },
            Segment::steady(300, 50.0, "Recovery"),
            Segment::steady(480, 98.0, "Interval 2"),
        ]);
        assert_eq!(
            build_cues(&w, &[])[0].detail.as_deref(),
            Some("Hold 95 rpm.")
        );
    }

    #[test]
    fn should_advise_pacing_on_a_ramped_effort() {
        let w = workout(vec![
            Segment::ramp(480, 85.0, 105.0, "Interval 1"),
            Segment::steady(300, 50.0, "Recovery"),
            Segment::steady(480, 98.0, "Interval 2"),
        ]);
        assert_eq!(
            build_cues(&w, &[])[0].detail.as_deref(),
            Some("Build steadily — don't go out too hard.")
        );
    }

    #[test]
    fn should_leave_a_plain_steady_rep_without_a_detail() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(cues[1].detail, None);
    }

    // ── Closing cues ─────────────────────────────────────────────────────────

    #[test]
    fn should_give_work_intervals_a_closing_cue() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(
            cues[1].closing.as_deref(),
            Some("Last 30 seconds — hold your form.")
        );
    }

    #[test]
    fn should_word_the_last_reps_closing_cue_differently() {
        let cues = build_cues(&intervals(), &[]);
        assert_eq!(
            cues[5].closing.as_deref(),
            Some("Last 30 seconds of the last rep — finish it.")
        );
    }

    #[test]
    fn should_not_give_rests_a_closing_cue() {
        let cues = build_cues(&intervals(), &[]);
        assert!(cues[0].closing.is_none());
        assert!(cues[2].closing.is_none());
        assert!(cues[6].closing.is_none());
    }

    #[test]
    fn should_not_give_a_short_effort_a_closing_cue() {
        // 60 s of work is over before a 30 s countdown says anything useful.
        let w = workout(vec![
            Segment::steady(60, 120.0, "Sprint 1"),
            Segment::steady(180, 45.0, "Recovery"),
            Segment::steady(60, 120.0, "Sprint 2"),
        ]);
        let cues = build_cues(&w, &[]);
        assert!(cues[0].closing.is_none());
        assert!(cues[2].closing.is_none());
    }

    #[test]
    fn should_give_a_90_second_effort_a_closing_cue() {
        let w = workout(vec![
            Segment::steady(90, 110.0, "Effort 1"),
            Segment::steady(180, 45.0, "Recovery"),
            Segment::steady(90, 110.0, "Effort 2"),
        ]);
        assert!(build_cues(&w, &[])[0].closing.is_some());
    }
}
