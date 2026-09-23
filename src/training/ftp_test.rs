//! Reading an FTP off a ramp test.
//!
//! The ramp test is the one way to get a real FTP number without waiting for
//! weeks of training to accumulate, which is what [`crate::training::ftp_detect`]
//! needs. It is ridden to exhaustion rather than completed: the rider climbs a
//! ladder of one-minute steps ([`Workout::ramp_test`]) and stops when they can
//! no longer hold one.
//!
//! FTP is [`RAMP_FTP_FRACTION`] of the best minute actually ridden. Read off
//! recorded power rather than the targets the trainer was given, which matters
//! in two cases that would otherwise flatter the rider: a test ridden with the
//! intensity dial turned down, and a final step the rider was given but never
//! held.
//!
//! Nothing here changes anything. The output is a suggestion the rider accepts
//! or dismisses on the summary page, and accepting it is what writes the number.

use crate::data::session::Session;
use crate::data::workout::{Segment, RAMP_FIRST_STEP_INDEX, RAMP_STEP_SECS};

/// FTP as a fraction of the best minute of a ramp test.
///
/// The standard ramp coefficient. A minute at maximum aerobic power is ridden
/// roughly a third above threshold, and 0.75 is the inverse of that.
pub const RAMP_FTP_FRACTION: f32 = 0.75;

/// Plausible bounds for a tested FTP, in watts.
///
/// Stored and sensor data are not trusted blindly (CLAUDE.md §5.1): a trainer
/// reporting a spurious four-figure watt reading for a minute would otherwise
/// produce a "suggestion" that wrecks every zone in the app. A result outside
/// these bounds is refused rather than clamped — clamping would put a number in
/// front of the rider that no part of their ride supports.
pub const MIN_TESTED_FTP: u32 = 50;
/// See [`MIN_TESTED_FTP`].
pub const MAX_TESTED_FTP: u32 = 500;

/// What a ramp test came out at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampResult {
    /// The FTP the test supports.
    pub new_ftp: u32,
    /// The FTP the test was ridden at, which the result is compared against.
    pub previous_ftp: u32,
    /// Mean power over the best minute of the ride.
    pub best_minute_watts: u32,
    /// Where that minute started, in seconds into the ride.
    pub best_minute_start_secs: u32,
    /// How many full ladder steps the rider got through. Zero for a test
    /// abandoned in the warm-up.
    pub steps_held: u32,
}

impl RampResult {
    /// Signed change from the FTP the test was ridden at, in percent.
    pub fn delta_pct(&self) -> f32 {
        if self.previous_ftp == 0 {
            return 0.0;
        }
        (self.new_ftp as f32 - self.previous_ftp as f32) / self.previous_ftp as f32 * 100.0
    }

    /// Whether the result is the same number the rider already has.
    pub fn is_unchanged(&self) -> bool {
        self.new_ftp == self.previous_ftp
    }
}

/// What a recorded ramp test says the rider's FTP is, or `None` when it says
/// nothing usable.
///
/// `None` for a ride too short to contain a minute, one that recorded no power,
/// one with no FTP stamped on it to compare against, or one whose best minute
/// implies an FTP outside [`MIN_TESTED_FTP`]..=[`MAX_TESTED_FTP`].
///
/// `segments` is the ladder the ride was ridden against, used only to count the
/// steps held; an empty slice simply yields zero steps.
pub fn ramp_result(session: &Session, segments: &[Segment]) -> Option<RampResult> {
    let previous_ftp = session.ftp_watts?;
    if previous_ftp == 0 {
        return None;
    }
    let (best_minute_watts, best_minute_start_secs) = best_minute(session)?;

    let new_ftp = (best_minute_watts as f32 * RAMP_FTP_FRACTION).round() as u32;
    if !(MIN_TESTED_FTP..=MAX_TESTED_FTP).contains(&new_ftp) {
        tracing::warn!(
            "Ramp test result of {new_ftp} W is outside {MIN_TESTED_FTP}–{MAX_TESTED_FTP} W \
             (best minute {best_minute_watts} W) — not offering it"
        );
        return None;
    }

    Some(RampResult {
        new_ftp,
        previous_ftp,
        best_minute_watts,
        best_minute_start_secs,
        steps_held: steps_held(session, segments),
    })
}

/// Mean power over the best minute of the ride, and the second it started.
///
/// Walked over elapsed time rather than over the recorded points, so a dropout
/// cannot compress a window: sixty points spanning five minutes of a stuttering
/// trainer are not a minute's effort. Seconds the trainer reported nothing for
/// count as zero, which pulls a window with a gap in it down — the safe
/// direction for a number that is about to become the rider's FTP.
fn best_minute(session: &Session) -> Option<(u32, u32)> {
    let window = RAMP_STEP_SECS as usize;
    let last = session
        .data_points
        .iter()
        .map(|p| p.elapsed_secs)
        .max()
        .unwrap_or(0) as usize;
    if last + 1 < window {
        return None;
    }
    // Indexed by elapsed second, so gaps stay gaps.
    let mut series = vec![0u32; last + 1];
    let mut any_power = false;
    for p in &session.data_points {
        if let Some(w) = p.power_watts {
            // Last writer wins if two points share a second, which a resumed
            // ride can produce; they carry the same reading in practice.
            series[p.elapsed_secs as usize] = w;
            any_power = true;
        }
    }
    if !any_power {
        return None;
    }

    // u64 so a rogue four-figure reading held for a minute cannot overflow the
    // running sum before the plausibility check downstream rejects it.
    let mut sum: u64 = series[..window].iter().map(|&w| w as u64).sum();
    let mut best = sum;
    let mut best_start = 0usize;
    for start in 1..=(series.len() - window) {
        sum -= series[start - 1] as u64;
        sum += series[start + window - 1] as u64;
        if sum > best {
            best = sum;
            best_start = start;
        }
    }
    Some(((best / window as u64) as u32, best_start as u32))
}

/// Full ladder steps the rider got through.
///
/// Counted from how far into the workout the ride reached, so a step the rider
/// entered but did not finish does not count. The warm-up occupies the segments
/// before [`RAMP_FIRST_STEP_INDEX`].
fn steps_held(session: &Session, segments: &[Segment]) -> u32 {
    let ridden = session
        .data_points
        .iter()
        .map(|p| p.elapsed_secs)
        .max()
        .map(|last| last + 1)
        .unwrap_or(0);
    let warmup: u32 = segments
        .iter()
        .take(RAMP_FIRST_STEP_INDEX)
        .map(|s| s.duration_secs)
        .sum();
    let in_ladder = ridden.saturating_sub(warmup);
    let ladder_steps = segments.len().saturating_sub(RAMP_FIRST_STEP_INDEX + 1) as u32;
    (in_ladder / RAMP_STEP_SECS).min(ladder_steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::DataPoint;
    use crate::data::workout::Workout;

    const FTP: u32 = 200;

    /// A ride whose per-second power is given directly, starting at second 0.
    fn ride(powers: &[Option<u32>]) -> Session {
        let mut session = Session::new(Some(1));
        session.ftp_watts = Some(FTP);
        for (i, &w) in powers.iter().enumerate() {
            session.data_points.push(DataPoint {
                elapsed_secs: i as u32,
                power_watts: w,
                target_watts: w,
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }
        session
    }

    /// `secs` seconds at a flat wattage.
    fn flat(secs: usize, watts: u32) -> Vec<Option<u32>> {
        vec![Some(watts); secs]
    }

    fn result(powers: &[Option<u32>]) -> Option<RampResult> {
        ramp_result(&ride(powers), &Workout::ramp_test().segments)
    }

    // ── The number ───────────────────────────────────────────────────────────

    #[test]
    fn should_read_ftp_as_three_quarters_of_the_best_minute() {
        let r = result(&flat(600, 300)).expect("ten minutes at 300 W is a usable test");
        assert_eq!(r.best_minute_watts, 300);
        assert_eq!(r.new_ftp, 225, "0.75 × 300");
    }

    #[test]
    fn should_find_the_best_minute_and_not_the_last_one() {
        // The shape of a real test: a hard step, then the one the rider failed.
        // Reading the final minute instead of the best would under-read FTP.
        let mut powers = flat(120, 200);
        powers.extend(flat(60, 320)); // the last step held — seconds 120..180
        powers.extend(flat(60, 180)); // came apart
        let r = result(&powers).expect("usable");
        assert_eq!(r.best_minute_watts, 320);
        assert_eq!(r.best_minute_start_secs, 120);
        assert_eq!(r.new_ftp, 240);
    }

    #[test]
    fn should_read_the_best_minute_across_a_step_boundary() {
        // A rider who fails half-way through a step still rode a best minute
        // straddling the two, and it is worth more than either whole step.
        let mut powers = flat(60, 300);
        powers.extend(flat(30, 340)); // half of the next step, then nothing left
        powers.extend(flat(30, 120));
        let r = result(&powers).expect("usable");
        // Best window is the 30 s at 300 plus the 30 s at 340.
        assert_eq!(r.best_minute_watts, 320);
        assert_eq!(r.best_minute_start_secs, 30);
    }

    #[test]
    fn should_round_the_result_rather_than_truncate_it() {
        // 0.75 × 299 = 224.25 → 224; 0.75 × 301 = 225.75 → 226. Truncation
        // would give 224 for both.
        assert_eq!(result(&flat(120, 299)).expect("usable").new_ftp, 224);
        assert_eq!(result(&flat(120, 301)).expect("usable").new_ftp, 226);
    }

    #[test]
    fn should_report_the_change_against_the_ftp_the_test_was_ridden_at() {
        let r = result(&flat(120, 320)).expect("usable");
        assert_eq!(r.previous_ftp, FTP);
        assert_eq!(r.new_ftp, 240);
        assert!((r.delta_pct() - 20.0).abs() < 0.01, "{}", r.delta_pct());
    }

    #[test]
    fn should_report_a_drop_as_a_negative_change() {
        let r = result(&flat(120, 200)).expect("usable");
        assert_eq!(r.new_ftp, 150);
        assert!(r.delta_pct() < 0.0, "{}", r.delta_pct());
    }

    #[test]
    fn should_notice_a_result_that_changes_nothing() {
        // 0.75 × 267 = 200.25 → 200, exactly the stored FTP.
        let r = result(&flat(120, 267)).expect("usable");
        assert_eq!(r.new_ftp, FTP);
        assert!(r.is_unchanged());
    }

    // ── Refusing to answer ───────────────────────────────────────────────────

    #[test]
    fn should_give_nothing_for_a_ride_shorter_than_a_minute() {
        assert_eq!(result(&flat(59, 300)), None);
        assert!(
            result(&flat(60, 300)).is_some(),
            "exactly a minute is enough — the boundary, not near it"
        );
    }

    #[test]
    fn should_give_nothing_for_a_ride_that_recorded_no_power_at_all() {
        assert_eq!(result(&vec![None; 600]), None);
    }

    #[test]
    fn should_give_nothing_for_an_empty_ride() {
        assert_eq!(result(&[]), None);
    }

    #[test]
    fn should_give_nothing_when_the_ride_carries_no_ftp_to_compare_against() {
        let mut session = ride(&flat(600, 300));
        session.ftp_watts = None;
        assert_eq!(ramp_result(&session, &Workout::ramp_test().segments), None);

        // A stamped zero is not an FTP either, and dividing by it would be a
        // NaN in the percentage the card prints.
        session.ftp_watts = Some(0);
        assert_eq!(ramp_result(&session, &Workout::ramp_test().segments), None);
    }

    #[test]
    fn should_refuse_a_result_from_an_implausible_power_reading() {
        // A rogue trainer reporting four figures for a minute (CLAUDE.md §5.1).
        // 0.75 × 65535 is far outside any human FTP, and clamping it to 500 W
        // would put a number in front of the rider that nothing supports.
        assert_eq!(result(&flat(120, 65535)), None);
        assert_eq!(result(&flat(120, 1000)), None, "0.75 × 1000 = 750 W");
    }

    #[test]
    fn should_refuse_a_result_too_low_to_be_an_ftp() {
        // A test abandoned in the warm-up, or a trainer reading near zero.
        assert_eq!(result(&flat(120, 40)), None, "0.75 × 40 = 30 W");
    }

    #[test]
    fn should_accept_a_result_exactly_on_each_plausibility_boundary() {
        // 0.75 × 667 = 500.25 → 500, the ceiling itself.
        assert_eq!(
            result(&flat(120, 667)).expect("500 W is allowed").new_ftp,
            MAX_TESTED_FTP
        );
        // 0.75 × 67 = 50.25 → 50, the floor itself.
        assert_eq!(
            result(&flat(120, 67)).expect("50 W is allowed").new_ftp,
            MIN_TESTED_FTP
        );
    }

    // ── Gaps in the recording ────────────────────────────────────────────────

    #[test]
    fn should_not_let_a_dropout_compress_the_best_minute() {
        // Thirty seconds at 400 W, a two-minute dropout, thirty more at 400.
        // Counted over recorded points that is a "minute" at 400 W and an FTP
        // of 300. Counted over elapsed time it is what it was: two half-minutes
        // with nothing in between.
        let mut powers = flat(30, 400);
        powers.extend(vec![None; 120]);
        powers.extend(flat(30, 400));
        let r = result(&powers).expect("long enough to hold a minute");
        assert!(
            r.best_minute_watts < 400,
            "a gap must not read as effort: got {} W",
            r.best_minute_watts
        );
        // The best window is the 30 s at 400 plus 30 s of silence: 200 W.
        assert_eq!(r.best_minute_watts, 200);
    }

    #[test]
    fn should_survive_a_ride_whose_seconds_are_not_contiguous() {
        // What a resumed ride can look like: elapsed seconds jumping a gap.
        let mut session = Session::new(Some(1));
        session.ftp_watts = Some(FTP);
        for i in (0..600u32).step_by(3) {
            session.data_points.push(DataPoint {
                elapsed_secs: i,
                power_watts: Some(300),
                target_watts: Some(300),
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }
        // Two seconds in three recorded nothing, so the best minute reads a
        // third of the power. It must not panic, and must not read 300 W.
        let r = ramp_result(&session, &Workout::ramp_test().segments).expect("long enough");
        assert!(r.best_minute_watts < 300);
    }

    // ── Steps held ───────────────────────────────────────────────────────────

    #[test]
    fn should_count_no_steps_for_a_test_abandoned_in_the_warm_up() {
        // The warm-up is ten minutes, so nine minutes in is no steps at all.
        let r = result(&flat(9 * 60, 150)).expect("usable");
        assert_eq!(r.steps_held, 0);
    }

    #[test]
    fn should_count_a_step_only_once_it_is_finished() {
        let warmup = 10 * 60;
        // Warm-up plus 59 seconds: the rider is *in* step one, not through it.
        assert_eq!(
            result(&flat(warmup + 59, 200)).expect("usable").steps_held,
            0
        );
        assert_eq!(
            result(&flat(warmup + 60, 200)).expect("usable").steps_held,
            1,
            "the boundary second completes the step"
        );
        assert_eq!(
            result(&flat(warmup + 61, 200)).expect("usable").steps_held,
            1
        );
    }

    #[test]
    fn should_count_the_steps_a_long_test_got_through() {
        let warmup = 10 * 60;
        let r = result(&flat(warmup + 9 * 60 + 20, 200)).expect("usable");
        assert_eq!(r.steps_held, 9);
    }

    #[test]
    fn should_not_count_more_steps_than_the_ladder_has() {
        // A rider who somehow rode the ladder out and into the cool-down.
        let r = result(&flat(60 * 60, 200)).expect("usable");
        let ladder_steps = Workout::ramp_test().segments.len() - 2;
        assert_eq!(r.steps_held as usize, ladder_steps);
    }

    // ── The ladder itself ────────────────────────────────────────────────────

    #[test]
    fn should_build_a_ladder_of_whole_minute_steps_that_climbs_throughout() {
        let w = Workout::ramp_test();
        assert!(w.is_ramp_test());
        let steps = &w.segments[RAMP_FIRST_STEP_INDEX..w.segments.len() - 1];
        assert!(
            steps.len() >= 12,
            "at least as long as the ladder it replaced"
        );
        for (i, seg) in steps.iter().enumerate() {
            assert_eq!(
                seg.duration_secs, RAMP_STEP_SECS,
                "step {i} is not a minute"
            );
            assert!(!seg.is_ramp(), "a step is held, not ramped: step {i}");
            if i > 0 {
                assert!(
                    seg.power_low_pct > steps[i - 1].power_low_pct,
                    "step {i} does not climb"
                );
            }
        }
    }

    #[test]
    fn should_leave_headroom_above_an_ftp_set_much_too_low() {
        // The point of the extended ladder. A rider fails a ramp at roughly
        // 4/3 of their true FTP, so a stored FTP 45 % below the truth means
        // failing at about 1.45 × 1.33 = 193 % of the stored number. The ladder
        // has to reach past that or it caps the answer at the wrong value.
        let w = Workout::ramp_test();
        let top = w
            .segments
            .iter()
            .map(|s| s.power_high_pct)
            .fold(0.0_f32, f32::max);
        assert!(top >= 193.0, "ladder tops out at {top}% of FTP");
    }

    #[test]
    fn should_not_ask_the_trainer_for_more_than_it_will_take() {
        // Every target must survive the ERG clamp (CLAUDE.md §5.1) for a strong
        // rider, or the top of the ladder is silently flat.
        let w = Workout::ramp_test();
        let strong_ftp = 400;
        let top = w
            .segments
            .iter()
            .map(|s| s.target_power_at(s.duration_secs, strong_ftp))
            .max()
            .expect("the ladder has segments");
        assert!(
            top <= crate::devices::ftms::MAX_ERG_TARGET_W as u32,
            "top step asks for {top} W at FTP {strong_ftp}"
        );
    }
}
