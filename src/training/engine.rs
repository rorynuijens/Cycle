use crate::data::{
    athlete::AthleteProfile,
    session::{DataPoint, LiveReadings, Session},
    workout::Workout, // Segment removed — was unused
};
use crate::devices::manager::DeviceCommand;
use async_channel::Sender;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Lowest the intensity dial goes, in percent of the workout as written.
pub const MIN_INTENSITY_PCT: u32 = 50;
/// Highest the intensity dial goes, in percent of the workout as written.
pub const MAX_INTENSITY_PCT: u32 = 150;
/// One press of the dial. Coarse on purpose: two presses is the usual move when
/// a session turns out to be too much, and it is hard to fumble mid-interval.
pub const INTENSITY_STEP_PCT: i32 = 5;

/// Seconds of pedalling in a row before a ride starts by itself — time to clip
/// in and get the legs turning before the clock and the targets begin.
pub const PRE_START_SECS: u32 = 10;

/// Whether one second's power reading counts as the rider pedalling.
///
/// A connected trainer reports 0 W while nobody is on it, so a reading alone
/// is not pedalling — only power is. Both ride pages start on this.
pub fn is_pedalling(power_watts: Option<u32>) -> bool {
    power_watts.is_some_and(|w| w > 0)
}

/// Wall-clock time a ride has spent paused.
///
/// A ride's saved duration is wall clock — `ended_at - started_at` — and TSS,
/// kilojoules and the integrity check are all read off it. Pausing stops the
/// ride's own clock, so without taking this back off at the end every minute
/// paused is saved as a minute ridden with no power. Shared by both ride
/// pages, which pause differently but end the same way.
#[derive(Debug, Clone, Default)]
pub struct PausedTime {
    /// When the current pause began, if the ride is paused now.
    since: Option<chrono::DateTime<chrono::Utc>>,
    /// Every pause already over.
    finished: chrono::Duration,
}

impl PausedTime {
    /// Start a pause. A second press while paused is ignored, so the pause is
    /// measured from when it actually began.
    pub fn pause(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.since.get_or_insert(now);
    }

    /// End the current pause, if there is one.
    pub fn resume(&mut self, now: chrono::DateTime<chrono::Utc>) {
        if let Some(since) = self.since.take() {
            self.finished += (now - since).max(chrono::Duration::zero());
        }
    }

    /// All the time paused up to `now`, counting a pause still in progress —
    /// a ride can end while paused.
    pub fn total(&self, now: chrono::DateTime<chrono::Utc>) -> chrono::Duration {
        let current = self
            .since
            .map(|since| (now - since).max(chrono::Duration::zero()))
            .unwrap_or_else(chrono::Duration::zero);
        self.finished + current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    Running,
    Paused,
    Completed,
}

/// Snapshot emitted every second for the UI to display.
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub state: EngineState,
    pub elapsed_secs: u32,
    pub remaining_secs: u32,
    pub segment_index: usize,
    pub segment_elapsed_secs: u32,
    pub segment_remaining_secs: u32,
    /// The target actually asked of the trainer — the plan with the dial applied.
    pub target_power_watts: u32,
    /// Where the intensity dial stands, as a percentage of the plan.
    pub intensity_pct: u32,
    pub readings: LiveReadings,
}

pub struct WorkoutEngine {
    pub workout: Workout,
    /// Shared with the rest of the app rather than owned, so an FTP edit in
    /// Preferences reaches the ERG targets of a ride already in progress.
    pub athlete: Rc<RefCell<AthleteProfile>>,
    pub session: Session,
    pub state: EngineState,
    start_instant: Option<Instant>,
    pause_offset: Duration,
    device_cmd_tx: Sender<DeviceCommand>,
    /// Max watts per second the ERG target may change. 0 = instant (no smoothing).
    pub erg_ramp_rate: u32,
    /// The intensity dial, as a percentage of the workout as written. Every
    /// target the trainer is given is scaled by this, so a rider who cannot hold
    /// the plan today can finish the session at 90 % instead of abandoning it.
    /// Read through [`WorkoutEngine::set_intensity_pct`] / [`Self::adjust_intensity`],
    /// which keep it inside [`MIN_INTENSITY_PCT`]..=[`MAX_INTENSITY_PCT`].
    intensity_pct: u32,
    current_erg_target: u32,
    /// Set by `resume_session` until the ride actually restarts. Keeps `start`
    /// from re-stamping `started_at` over the original ride's start time.
    resuming: bool,
    /// Wall-clock time the ride was not being ridden for because the app was
    /// closed. Subtracted at `stop` so the gap does not inflate the ride's
    /// duration, and through it the TSS.
    interruption: chrono::Duration,
    /// Time paused mid-ride — see [`PausedTime`]. Taken off at `stop` alongside
    /// `interruption`, which covers the app being closed rather than paused.
    paused: PausedTime,
}

impl WorkoutEngine {
    pub fn new(
        workout: Workout,
        athlete: Rc<RefCell<AthleteProfile>>,
        device_cmd_tx: Sender<DeviceCommand>,
    ) -> Self {
        let mut session = Session::new(Some(workout.id));
        // Stamp the FTP the ride is executed at — FTP detection needs it to
        // interpret targets after the profile FTP changes.
        session.ftp_watts = Some(athlete.borrow().ftp_watts);
        // And stamp what kind of ride it is, for the same reader: a test is
        // built to end in failure, so counting it as evidence would read a
        // successful one as a reason to ease FTP. Recorded on the ride rather
        // than looked up later — see `Session::is_ftp_test`.
        session.is_ftp_test = workout.is_ramp_test();
        Self {
            session,
            workout,
            athlete,
            state: EngineState::Idle,
            start_instant: None,
            pause_offset: Duration::ZERO,
            device_cmd_tx,
            erg_ramp_rate: 25,
            intensity_pct: 100,
            current_erg_target: 0,
            resuming: false,
            interruption: chrono::Duration::zero(),
            paused: PausedTime::default(),
        }
    }

    pub fn start(&mut self) {
        self.start_instant = Some(Instant::now());
        if self.resuming {
            // Picking a ride back up: keep the original start time — moving it
            // would misdate the ride and break the Intervals.icu match, which
            // pairs on when the ride began. Instead measure the gap between where
            // the ride left off and now, and take it back off at `stop`.
            self.resuming = false;
            let left_off = self.session.started_at
                + chrono::Duration::seconds(self.pause_offset.as_secs() as i64);
            self.interruption = (chrono::Utc::now() - left_off).max(chrono::Duration::zero());
            tracing::info!(
                "Workout resumed at {}s: {}",
                self.pause_offset.as_secs(),
                self.workout.name
            );
        } else {
            // The session was created when the workout was selected, which can be long
            // before the first pedal stroke. Stamp the real start so the ride's duration
            // (and the TSS derived from it) covers only time actually ridden, and so it
            // lines up with what a head unit or Intervals.icu records for the same ride.
            self.session.started_at = chrono::Utc::now();
            tracing::info!("Workout started: {}", self.workout.name);
        }
        self.state = EngineState::Running;
    }

    /// Pick up a ride that was interrupted, at the second it stopped recording.
    ///
    /// The engine goes back to `Idle`, so the player's usual ten-second power gate
    /// runs before the ride restarts — the rider gets time to clip in. Elapsed
    /// time continues from the last recorded second rather than from zero.
    pub fn resume_session(&mut self, workout: Workout, session: Session) {
        // Resume at the second *after* the last one recorded — starting on it
        // would record that second twice.
        let elapsed = session
            .data_points
            .last()
            .map(|p| p.elapsed_secs + 1)
            .unwrap_or(0);
        self.session = session;
        // Re-derived rather than trusted: a ride checkpointed before the flag
        // existed carries `false` whatever it actually was.
        self.session.is_ftp_test = workout.is_ramp_test();
        self.workout = workout;
        self.state = EngineState::Idle;
        self.start_instant = None;
        self.pause_offset = Duration::from_secs(elapsed as u64);
        // A ride picked back up starts at the plan again: the dial is a judgement
        // about how the legs feel, and nothing recorded says where it stood.
        self.intensity_pct = 100;
        self.current_erg_target = 0;
        self.resuming = true;
        self.interruption = chrono::Duration::zero();
        self.paused = PausedTime::default();
    }

    pub fn pause(&mut self) {
        self.pause_at(chrono::Utc::now());
    }

    fn pause_at(&mut self, now: chrono::DateTime<chrono::Utc>) {
        if self.state == EngineState::Running {
            if let Some(start) = self.start_instant {
                self.pause_offset += start.elapsed();
                self.start_instant = None;
            }
            self.paused.pause(now);
            self.state = EngineState::Paused;
            self.release_trainer();
        }
    }

    pub fn resume(&mut self) {
        self.resume_at(chrono::Utc::now());
    }

    fn resume_at(&mut self, now: chrono::DateTime<chrono::Utc>) {
        if self.state == EngineState::Paused {
            self.paused.resume(now);
            self.start_instant = Some(Instant::now());
            self.state = EngineState::Running;
        }
    }

    /// Where the intensity dial stands, in percent of the workout as written.
    pub fn intensity_pct(&self) -> u32 {
        self.intensity_pct
    }

    /// Move the intensity dial to `pct`, clamped to the dial's range.
    ///
    /// Takes effect on the next tick — the trainer is only spoken to once a
    /// second, and the existing ERG ramp still governs how fast it gets there.
    pub fn set_intensity_pct(&mut self, pct: u32) {
        self.intensity_pct = pct.clamp(MIN_INTENSITY_PCT, MAX_INTENSITY_PCT);
    }

    /// Move the intensity dial by `delta` percentage points, clamped to range.
    pub fn adjust_intensity(&mut self, delta: i32) {
        let next = (self.intensity_pct as i32 + delta).max(0) as u32;
        self.set_intensity_pct(next);
    }

    /// A planned target with the dial applied.
    ///
    /// A rest stays a rest: 0 W scales to 0 W rather than to some small nonzero
    /// target, which would have the trainer pushing back during recovery.
    fn dialled(&self, planned_watts: u32) -> u32 {
        if planned_watts == 0 || self.intensity_pct == 100 {
            return planned_watts;
        }
        (planned_watts as f32 * self.intensity_pct as f32 / 100.0).round() as u32
    }

    /// Skip to the start of the next segment.
    pub fn skip_to_next_segment(&mut self) {
        if !matches!(self.state, EngineState::Running | EngineState::Paused) {
            return;
        }
        let elapsed = self.elapsed_secs();
        let (seg_idx, _) = self.segment_at(elapsed);
        let next_start: u32 = self
            .workout
            .segments
            .iter()
            .take(seg_idx + 1)
            .map(|s| s.duration_secs)
            .sum();
        let target = next_start.min(self.workout.duration_secs);
        let target_dur = Duration::from_secs(target as u64);
        if self.state == EngineState::Running {
            self.start_instant = Some(Instant::now());
        }
        self.pause_offset = target_dur;
    }

    /// Reset the engine with a new workout, discarding any in-progress session.
    /// Used when the user picks a different workout from the library.
    pub fn reset_with_workout(&mut self, workout: Workout) {
        let workout_id = Some(workout.id);
        self.session = Session::new(workout_id);
        self.session.ftp_watts = Some(self.athlete.borrow().ftp_watts);
        self.session.is_ftp_test = workout.is_ramp_test();
        self.workout = workout;
        self.state = EngineState::Idle;
        self.start_instant = None;
        self.pause_offset = Duration::ZERO;
        self.intensity_pct = 100;
        self.current_erg_target = 0;
        self.resuming = false;
        self.interruption = chrono::Duration::zero();
        self.paused = PausedTime::default();
    }

    pub fn stop(&mut self) {
        self.stop_at(chrono::Utc::now());
    }

    fn stop_at(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.state = EngineState::Completed;
        self.release_trainer();
        // Discount any time the ride spent interrupted or paused, so a session
        // picked back up an hour later — or left paused for ten minutes — is not
        // recorded as longer than it was ridden.
        self.session.ended_at = Some(now - self.interruption - self.paused.total(now));
    }

    /// Tell the trainer to let go. It holds whatever it was last asked for, so
    /// without this a rider stepping off — paused, or finished — meets the last
    /// interval's resistance when they get back on. Only a running ride sends
    /// targets, so this stays in force until the ride resumes.
    fn release_trainer(&self) {
        let _ = self
            .device_cmd_tx
            .try_send(DeviceCommand::SetTargetPower { watts: 0 });
    }

    pub fn elapsed_secs(&self) -> u32 {
        let base = self.pause_offset;
        let live = self.start_instant.map(|i| i.elapsed()).unwrap_or_default();
        (base + live).as_secs() as u32
    }

    /// Called once per second by the GLib timer on the main thread.
    pub fn tick(&mut self, readings: LiveReadings) -> EngineSnapshot {
        let elapsed = self.elapsed_secs();
        let (seg_idx, seg_elapsed) = self.segment_at(elapsed);
        let total_duration = self.workout.duration_secs;

        if elapsed >= total_duration && self.state == EngineState::Running {
            self.stop();
        }

        // `recorded_target` is None once past the last segment — recorded data
        // points distinguish "no target" from "target of 0 W" for FTP detection.
        //
        // It carries the *dialled* target, not the plan: that is what the trainer
        // was actually asked for, so compliance reads as holding what was asked,
        // and the FTP detector sees an honest lower target rather than a segment
        // the rider was never given a chance to hit. See docs/ftp-detection.md.
        let (target_watts, seg_remaining, recorded_target) =
            if let Some(seg) = self.workout.segments.get(seg_idx) {
                let ftp = self.athlete.borrow().ftp_watts;
                let target = self.dialled(seg.target_power_at(seg_elapsed, ftp));
                let remaining = seg.duration_secs.saturating_sub(seg_elapsed);
                (target, remaining, Some(target))
            } else {
                (0, 0, None)
            };

        if self.state == EngineState::Running && target_watts > 0 {
            let tx = self.device_cmd_tx.clone();
            // Ramp current_erg_target toward target_watts at most erg_ramp_rate W/s.
            // erg_ramp_rate == 0 means instant (no smoothing).
            let smoothed = if self.erg_ramp_rate == 0 || self.current_erg_target == 0 {
                target_watts
            } else if target_watts > self.current_erg_target {
                (self.current_erg_target + self.erg_ramp_rate).min(target_watts)
            } else {
                self.current_erg_target
                    .saturating_sub(self.erg_ramp_rate)
                    .max(target_watts)
            };
            self.current_erg_target = smoothed;
            // Clamp to 1000 W before sending — CLAUDE.md §5.1.
            let watts = smoothed.min(1000) as u16;
            let _ = tx.try_send(DeviceCommand::SetTargetPower { watts });
        }

        if self.state == EngineState::Running {
            self.session.data_points.push(DataPoint {
                elapsed_secs: elapsed,
                power_watts: readings.power_watts,
                target_watts: recorded_target,
                heart_rate_bpm: readings.heart_rate_bpm,
                cadence_rpm: readings.cadence_rpm,
                speed_kmh: readings.speed_kmh,
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }

        EngineSnapshot {
            state: self.state,
            elapsed_secs: elapsed,
            remaining_secs: total_duration.saturating_sub(elapsed),
            segment_index: seg_idx,
            segment_elapsed_secs: seg_elapsed,
            segment_remaining_secs: seg_remaining,
            target_power_watts: target_watts,
            intensity_pct: self.intensity_pct,
            readings,
        }
    }

    fn segment_at(&self, elapsed: u32) -> (usize, u32) {
        let mut remaining = elapsed;
        for (i, seg) in self.workout.segments.iter().enumerate() {
            if remaining < seg.duration_secs {
                return (i, remaining);
            }
            remaining -= seg.duration_secs;
        }
        let last = self.workout.segments.len().saturating_sub(1);
        (
            last,
            self.workout
                .segments
                .last()
                .map(|s| s.duration_secs)
                .unwrap_or(0),
        )
    }

    pub fn format_duration(secs: u32) -> String {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::DataPoint;

    fn make_engine() -> WorkoutEngine {
        // The engine sends ERG commands but the tests discard them.
        let (cmd_tx, _cmd_rx) = async_channel::bounded(16);
        WorkoutEngine::new(
            Workout::sample_threshold(),
            Rc::new(RefCell::new(AthleteProfile {
                ftp_watts: 250,
                ..AthleteProfile::default()
            })),
            cmd_tx,
        )
    }

    /// Build an engine alongside the profile cell it shares, so a test can edit
    /// the profile the way Preferences does and see what the engine makes of it.
    fn engine_with_athlete() -> (WorkoutEngine, Rc<RefCell<AthleteProfile>>) {
        let (cmd_tx, _cmd_rx) = async_channel::bounded(16);
        let athlete = Rc::new(RefCell::new(AthleteProfile {
            ftp_watts: 250,
            ..AthleteProfile::default()
        }));
        let engine = WorkoutEngine::new(Workout::sample_threshold(), Rc::clone(&athlete), cmd_tx);
        (engine, athlete)
    }

    #[test]
    fn should_scale_targets_to_a_new_ftp_without_rebuilding_the_engine() {
        let (mut engine, athlete) = engine_with_athlete();
        engine.start();
        let before = engine.tick(LiveReadings::default()).target_power_watts;
        assert!(before > 0, "sample workout should open with a real target");

        // Exactly what saving Preferences does — one write to the shared cell.
        athlete.borrow_mut().ftp_watts = 500;

        let after = engine.tick(LiveReadings::default()).target_power_watts;
        assert_eq!(
            after,
            before * 2,
            "doubling FTP should double the ERG target for the same segment"
        );
    }

    #[test]
    fn should_start_the_dial_at_the_plan() {
        assert_eq!(make_engine().intensity_pct(), 100);
    }

    #[test]
    fn should_scale_the_target_and_what_is_recorded_together_when_dialled_down() {
        let mut engine = make_engine();
        engine.start();
        let planned = engine.tick(LiveReadings::default()).target_power_watts;
        assert!(planned > 0, "sample workout should open with a real target");

        engine.set_intensity_pct(90);
        let snap = engine.tick(LiveReadings::default());

        let expected = (planned as f32 * 0.9).round() as u32;
        assert_eq!(snap.target_power_watts, expected);
        assert_eq!(snap.intensity_pct, 90);
        // What the trainer was asked for is what the ride records — compliance and
        // FTP detection both read this rather than the plan.
        let last = engine
            .session
            .data_points
            .last()
            .expect("a recorded second");
        assert_eq!(last.target_watts, Some(expected));
    }

    #[test]
    fn should_leave_a_rest_as_a_rest_when_dialled_down() {
        let mut engine = make_engine();
        engine.set_intensity_pct(50);
        // Scaling a 0 W rest must not push the trainer back during recovery.
        assert_eq!(engine.dialled(0), 0);
    }

    #[test]
    fn should_stop_at_the_bottom_of_the_dial() {
        let mut engine = make_engine();
        engine.set_intensity_pct(10);
        assert_eq!(engine.intensity_pct(), MIN_INTENSITY_PCT);
    }

    #[test]
    fn should_stop_at_the_top_of_the_dial() {
        let mut engine = make_engine();
        engine.set_intensity_pct(400);
        assert_eq!(engine.intensity_pct(), MAX_INTENSITY_PCT);
    }

    #[test]
    fn should_hold_at_the_bottom_when_pressed_down_again() {
        let mut engine = make_engine();
        engine.set_intensity_pct(MIN_INTENSITY_PCT);
        // Underflow territory: the dial is u32 and the step is negative.
        engine.adjust_intensity(-INTENSITY_STEP_PCT);
        assert_eq!(engine.intensity_pct(), MIN_INTENSITY_PCT);
    }

    #[test]
    fn should_step_the_dial_by_one_press() {
        let mut engine = make_engine();
        engine.adjust_intensity(-INTENSITY_STEP_PCT);
        assert_eq!(engine.intensity_pct(), 95);
        engine.adjust_intensity(INTENSITY_STEP_PCT);
        assert_eq!(engine.intensity_pct(), 100);
    }

    #[test]
    fn should_return_the_dial_to_the_plan_when_reset_with_a_new_workout() {
        let mut engine = make_engine();
        engine.set_intensity_pct(85);
        engine.reset_with_workout(Workout::sample_threshold());
        assert_eq!(engine.intensity_pct(), 100);
    }

    #[test]
    fn should_return_the_dial_to_the_plan_when_a_ride_is_resumed() {
        let mut engine = make_engine();
        engine.set_intensity_pct(85);
        engine.resume_session(Workout::sample_threshold(), ridden_for(600));
        assert_eq!(engine.intensity_pct(), 100);
    }

    #[test]
    fn should_stamp_a_resumed_ride_with_the_ftp_in_force_when_it_restarts() {
        let (mut engine, athlete) = engine_with_athlete();
        athlete.borrow_mut().ftp_watts = 300;
        engine.reset_with_workout(Workout::sample_threshold());
        assert_eq!(engine.session.ftp_watts, Some(300));
    }

    fn ridden_for(secs: u32) -> Session {
        let mut session = Session::new(Some(1));
        for i in 0..secs {
            session.data_points.push(DataPoint {
                elapsed_secs: i,
                power_watts: Some(200),
                target_watts: None,
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

    #[test]
    fn should_wait_for_the_power_gate_after_resuming() {
        let mut engine = make_engine();
        engine.resume_session(Workout::sample_threshold(), ridden_for(600));
        // Idle, so the player's ten-second countdown runs before riding restarts.
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[test]
    fn should_resume_at_the_second_after_the_last_one_recorded() {
        let mut engine = make_engine();
        engine.resume_session(Workout::sample_threshold(), ridden_for(600));
        // Last recorded second is 599, so the ride picks up at 600 rather than
        // recording 599 a second time.
        assert_eq!(engine.elapsed_secs(), 600);
    }

    #[test]
    fn should_keep_the_original_start_time_when_resuming() {
        let mut engine = make_engine();
        let mut session = ridden_for(600);
        let original_start = chrono::Utc::now() - chrono::Duration::hours(2);
        session.started_at = original_start;
        engine.resume_session(Workout::sample_threshold(), session);
        engine.start();
        // Re-stamping would misdate the ride and break the Intervals.icu match,
        // which pairs activities on when they began.
        assert_eq!(engine.session.started_at, original_start);
    }

    #[test]
    fn should_not_count_the_interruption_in_a_resumed_ride() {
        let mut engine = make_engine();
        let mut session = ridden_for(600);
        // Ridden for 10 minutes, then interrupted an hour ago.
        session.started_at = chrono::Utc::now() - chrono::Duration::minutes(70);
        engine.resume_session(Workout::sample_threshold(), session);
        engine.start();
        engine.stop();

        // Without discounting the gap this would read as ~70 minutes.
        let duration = engine.session.duration_secs();
        assert!(
            (595..=605).contains(&duration),
            "resumed ride reported {duration}s, expected about 600"
        );
    }

    #[test]
    fn should_stamp_a_fresh_start_time_for_a_ride_that_is_not_resumed() {
        let mut engine = make_engine();
        engine.session.started_at = chrono::Utc::now() - chrono::Duration::hours(3);
        engine.start();
        let age = (chrono::Utc::now() - engine.session.started_at).num_seconds();
        assert!(
            age < 5,
            "a normal start must re-stamp started_at, got {age}s"
        );
    }

    #[test]
    fn should_forget_a_pending_resume_when_reset_with_a_new_workout() {
        let mut engine = make_engine();
        engine.resume_session(Workout::sample_threshold(), ridden_for(600));
        engine.reset_with_workout(Workout::sample_threshold());
        assert_eq!(engine.elapsed_secs(), 0);
        engine.start();
        // Picking a different workout must behave like any fresh ride.
        let age = (chrono::Utc::now() - engine.session.started_at).num_seconds();
        assert!(age < 5, "reset must clear the resume flag, got {age}s");
    }

    // ── Pausing, resuming and skipping ──────────────────────────────────────
    //
    // Mutation testing found this corner empty: `pause`, `resume` and
    // `skip_to_next_segment` could each be replaced by an empty function body
    // and the whole suite still passed. The tests above all exercise the
    // intensity dial and the resume bookkeeping; nothing drove the state
    // machine the player actually runs on.

    // ── What counts as pedalling ────────────────────────────────────────────
    // A trainer reports 0 W while the rider stands beside it. The workout
    // player counted that as pedalling, so a workout started itself ten
    // seconds after the trainer connected; the route player did not.

    #[test]
    fn should_not_count_a_trainer_reporting_zero_as_pedalling() {
        assert!(!is_pedalling(Some(0)));
    }

    #[test]
    fn should_not_count_no_reading_as_pedalling() {
        assert!(!is_pedalling(None));
    }

    #[test]
    fn should_count_a_single_watt_as_pedalling() {
        assert!(is_pedalling(Some(1)));
    }

    // ── Paused time is not ride time ────────────────────────────────────────
    // A pause stopped the workout clock but not the wall clock the saved ride
    // is measured by, so ten minutes paused in an hour saved seventy minutes,
    // inflated TSS by 17 % and marked the ride untrusted for missing power.

    fn secs(n: i64) -> chrono::Duration {
        chrono::Duration::seconds(n)
    }

    #[test]
    fn should_not_count_a_pause_in_the_ride_duration() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        engine.pause_at(t0 + secs(600));
        engine.resume_at(t0 + secs(1200));
        engine.stop_at(t0 + secs(1800));
        assert_eq!(engine.session.duration_secs(), 1200);
    }

    #[test]
    fn should_not_count_a_pause_the_ride_ends_in() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        engine.pause_at(t0 + secs(600));
        engine.stop_at(t0 + secs(1500));
        assert_eq!(engine.session.duration_secs(), 600);
    }

    #[test]
    fn should_not_count_any_of_several_pauses() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        engine.pause_at(t0 + secs(100));
        engine.resume_at(t0 + secs(160));
        engine.pause_at(t0 + secs(400));
        engine.resume_at(t0 + secs(700));
        engine.stop_at(t0 + secs(1000));
        assert_eq!(engine.session.duration_secs(), 1000 - 60 - 300);
    }

    #[test]
    fn should_not_count_a_second_pause_press_twice() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        engine.pause_at(t0 + secs(100));
        // Ignored: the ride is already paused, so the pause began at 100 s.
        engine.pause_at(t0 + secs(300));
        engine.resume_at(t0 + secs(400));
        engine.stop_at(t0 + secs(1000));
        assert_eq!(engine.session.duration_secs(), 700);
    }

    #[test]
    fn should_not_carry_a_pause_into_the_next_workout() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        engine.pause_at(t0 + secs(100));
        engine.reset_with_workout(Workout::sample_threshold());
        engine.start();
        let t1 = engine.session.started_at;
        engine.stop_at(t1 + secs(600));
        assert_eq!(engine.session.duration_secs(), 600);
    }

    #[test]
    fn should_count_nothing_for_a_resume_with_no_pause() {
        let t0 = chrono::Utc::now();
        let mut paused = PausedTime::default();
        paused.resume(t0 + secs(60));
        assert_eq!(paused.total(t0 + secs(120)), secs(0));
    }

    #[test]
    fn should_never_count_negative_time_when_the_clock_steps_back() {
        // The wall clock can jump backwards (NTP, a suspend); a pause must
        // never *add* ride time.
        let t0 = chrono::Utc::now();
        let mut paused = PausedTime::default();
        paused.pause(t0);
        assert_eq!(paused.total(t0 - secs(30)), secs(0));
        paused.resume(t0 - secs(30));
        assert_eq!(paused.total(t0 + secs(600)), secs(0));
    }

    #[test]
    fn should_count_a_pause_up_to_the_second_it_is_read() {
        let t0 = chrono::Utc::now();
        let mut paused = PausedTime::default();
        paused.pause(t0);
        assert_eq!(paused.total(t0 + secs(45)), secs(45));
    }

    #[test]
    fn should_trust_a_ride_with_a_deliberate_pause_in_it() {
        let mut engine = make_engine();
        engine.start();
        let t0 = engine.session.started_at;
        // Twenty minutes ridden with power throughout, ten paused in the middle.
        for i in 0..1200u32 {
            engine.session.data_points.push(DataPoint {
                elapsed_secs: i,
                power_watts: Some(150 + i % 60),
                target_watts: None,
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }
        engine.pause_at(t0 + secs(600));
        engine.resume_at(t0 + secs(1200));
        engine.stop_at(t0 + secs(1800));
        let verdict = crate::training::integrity::check(&engine.session);
        assert!(
            verdict.is_trusted(),
            "a pause is not missing power: {:?}",
            verdict.concerns()
        );
    }

    // ── Letting the trainer go ──────────────────────────────────────────────
    // Nothing ever told the trainer the ride had stopped: it held the last
    // interval's target through a pause and after the finish, so a rider
    // stepping off mid-VO₂ rep met 300 W when they clipped back in.

    fn make_engine_with_commands() -> (WorkoutEngine, async_channel::Receiver<DeviceCommand>) {
        let (cmd_tx, cmd_rx) = async_channel::bounded(16);
        let engine = WorkoutEngine::new(
            Workout::sample_threshold(),
            Rc::new(RefCell::new(AthleteProfile {
                ftp_watts: 250,
                ..AthleteProfile::default()
            })),
            cmd_tx,
        );
        (engine, cmd_rx)
    }

    /// Every ERG target sent so far, in order.
    fn targets_sent(rx: &async_channel::Receiver<DeviceCommand>) -> Vec<u16> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|cmd| match cmd {
                DeviceCommand::SetTargetPower { watts } => Some(watts),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn should_release_the_trainer_when_the_ride_is_paused() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.start();
        engine.tick(LiveReadings::default());
        let before = targets_sent(&rx);
        assert!(
            before.last().is_some_and(|w| *w > 0),
            "the ride was holding a target"
        );
        engine.pause();
        assert_eq!(targets_sent(&rx), vec![0]);
    }

    #[test]
    fn should_release_the_trainer_when_the_ride_is_ended() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.start();
        engine.tick(LiveReadings::default());
        targets_sent(&rx);
        engine.stop();
        assert_eq!(targets_sent(&rx), vec![0]);
    }

    #[test]
    fn should_release_the_trainer_when_the_workout_runs_out() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.start();
        // Past the last second of the workout: the tick completes the ride.
        engine.pause_offset = Duration::from_secs(engine.workout.duration_secs as u64);
        let snapshot = engine.tick(LiveReadings::default());
        assert_eq!(snapshot.state, EngineState::Completed);
        assert_eq!(targets_sent(&rx), vec![0]);
    }

    #[test]
    fn should_release_the_trainer_when_ended_while_paused() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.start();
        engine.pause();
        targets_sent(&rx);
        engine.stop();
        assert_eq!(targets_sent(&rx), vec![0]);
    }

    #[test]
    fn should_not_touch_the_trainer_for_a_pause_before_the_ride_starts() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.pause();
        assert!(targets_sent(&rx).is_empty());
    }

    #[test]
    fn should_hold_the_target_again_when_the_ride_resumes() {
        let (mut engine, rx) = make_engine_with_commands();
        engine.start();
        engine.tick(LiveReadings::default());
        let held = *targets_sent(&rx).last().expect("a target was sent");
        engine.pause();
        engine.resume();
        targets_sent(&rx);
        engine.tick(LiveReadings::default());
        assert_eq!(targets_sent(&rx), vec![held]);
    }

    #[test]
    fn should_stop_the_clock_when_a_ride_is_paused() {
        let mut engine = make_engine();
        engine.start();
        engine.pause();
        assert_eq!(engine.state, EngineState::Paused);
        assert!(
            engine.start_instant.is_none(),
            "a paused ride must not leave a clock running"
        );
    }

    #[test]
    fn should_keep_the_time_already_ridden_when_pausing() {
        let mut engine = make_engine();
        // Ten minutes in, so a pause that loses time is visible in whole
        // seconds rather than in the microseconds the test itself takes.
        engine.pause_offset = Duration::from_secs(600);
        engine.start();
        engine.pause();
        // A range, not an equality: the clock really is running between those
        // two calls, and a test that fails when the machine stalls for a second
        // is a test that teaches its suite to be re-run rather than believed.
        let elapsed = engine.elapsed_secs();
        assert!(
            (600..=601).contains(&elapsed),
            "expected the ten minutes already ridden, got {elapsed}"
        );
    }

    #[test]
    fn should_start_the_clock_again_when_a_paused_ride_resumes() {
        let mut engine = make_engine();
        engine.start();
        engine.pause();
        engine.resume();
        assert_eq!(engine.state, EngineState::Running);
        assert!(
            engine.start_instant.is_some(),
            "a running ride needs a clock to measure from"
        );
    }

    #[test]
    fn should_not_carry_paused_time_into_the_ride_when_resuming() {
        let mut engine = make_engine();
        engine.pause_offset = Duration::from_secs(600);
        engine.start();
        engine.pause();
        engine.resume();
        let elapsed = engine.elapsed_secs();
        assert!(
            (600..=601).contains(&elapsed),
            "resuming must not add the time spent paused, got {elapsed}"
        );
    }

    #[test]
    fn should_ignore_a_pause_before_the_ride_has_started() {
        let mut engine = make_engine();
        engine.pause();
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[test]
    fn should_ignore_a_resume_on_a_ride_that_is_not_paused() {
        let mut engine = make_engine();
        engine.resume();
        assert_eq!(
            engine.state,
            EngineState::Idle,
            "resume picks a paused ride back up; it does not start one"
        );
    }

    #[test]
    fn should_jump_to_the_start_of_the_next_segment() {
        // `sample_threshold` opens with a 600 s warm-up.
        let mut engine = make_engine();
        engine.start();
        engine.pause();
        engine.skip_to_next_segment();
        assert_eq!(engine.elapsed_secs(), 600);
        assert_eq!(engine.segment_at(engine.elapsed_secs()), (1, 0));
    }

    #[test]
    fn should_skip_one_segment_at_a_time() {
        // 600 s warm-up, then a 480 s interval: two skips land at 1080 s.
        let mut engine = make_engine();
        engine.start();
        engine.pause();
        engine.skip_to_next_segment();
        engine.skip_to_next_segment();
        assert_eq!(engine.elapsed_secs(), 1080);
        assert_eq!(engine.segment_at(engine.elapsed_secs()), (2, 0));
    }

    #[test]
    fn should_not_set_a_paused_ride_running_by_skipping() {
        let mut engine = make_engine();
        engine.start();
        engine.pause();
        engine.skip_to_next_segment();
        assert_eq!(engine.state, EngineState::Paused);
        assert!(
            engine.start_instant.is_none(),
            "skipping a segment is not a reason to un-pause"
        );
    }

    #[test]
    fn should_ignore_a_skip_before_the_ride_has_started() {
        let mut engine = make_engine();
        engine.skip_to_next_segment();
        assert_eq!(engine.elapsed_secs(), 0);
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[test]
    fn should_return_the_segment_and_its_offset_at_an_elapsed_time() {
        let engine = make_engine();
        assert_eq!(engine.segment_at(0), (0, 0));
        assert_eq!(engine.segment_at(599), (0, 599));
        assert_eq!(engine.segment_at(600), (1, 0));
        assert_eq!(engine.segment_at(601), (1, 1));
    }

    #[test]
    fn should_hold_at_the_last_segment_past_the_end_of_the_workout() {
        let engine = make_engine();
        let last = engine.workout.segments.len() - 1;
        // The closing cool-down, run all the way out.
        assert_eq!(engine.segment_at(u32::MAX), (last, 600));
    }

    // ── What tick asks the trainer for ──────────────────────────────────────
    //
    // `tick` runs once a second for the whole ride and decides the watts the
    // trainer is actually told to hold. Ten of its comparisons could be flipped
    // without a test objecting, including every branch of the ERG ramp.

    /// Sample workout, warm-up first: 50 % of a 250 W FTP is a 125 W target.
    const WARM_UP_TARGET_W: u32 = 125;

    #[test]
    fn should_stop_a_ride_that_has_run_out_of_workout() {
        let mut engine = make_engine();
        engine.pause_offset = Duration::from_secs(engine.workout.duration_secs as u64);
        engine.start();
        let snapshot = engine.tick(LiveReadings::default());
        assert_eq!(snapshot.state, EngineState::Completed);
    }

    #[test]
    fn should_keep_running_a_second_short_of_the_end() {
        let mut engine = make_engine();
        engine.pause_offset = Duration::from_secs(engine.workout.duration_secs as u64 - 2);
        engine.start();
        assert_eq!(
            engine.tick(LiveReadings::default()).state,
            EngineState::Running
        );
    }

    #[test]
    fn should_send_the_first_target_without_ramping_up_to_it() {
        // A held target of 0 means "nothing asked for yet" rather than "0 W",
        // so the first target of the ride goes out as it stands. Ramping up
        // from zero would leave the rider soft-pedalling through the opening.
        let mut engine = make_engine();
        engine.erg_ramp_rate = 20;
        engine.start();
        engine.tick(LiveReadings::default());
        assert_eq!(engine.current_erg_target, WARM_UP_TARGET_W);
    }

    #[test]
    fn should_climb_to_a_higher_target_at_the_ramp_rate() {
        let mut engine = make_engine();
        engine.erg_ramp_rate = 20;
        engine.current_erg_target = 100;
        engine.start();
        engine.tick(LiveReadings::default());
        assert_eq!(engine.current_erg_target, 120, "one second of ramp is 20 W");
        engine.tick(LiveReadings::default());
        assert_eq!(
            engine.current_erg_target, WARM_UP_TARGET_W,
            "the last 5 W is less than a step, so it lands exactly rather than overshooting"
        );
    }

    #[test]
    fn should_fall_to_a_lower_target_at_the_ramp_rate() {
        let mut engine = make_engine();
        engine.erg_ramp_rate = 20;
        engine.current_erg_target = 200;
        engine.start();
        engine.tick(LiveReadings::default());
        assert_eq!(engine.current_erg_target, 180);
    }

    #[test]
    fn should_go_straight_to_the_target_when_ramping_is_off() {
        let mut engine = make_engine();
        engine.erg_ramp_rate = 0;
        engine.current_erg_target = 300;
        engine.start();
        engine.tick(LiveReadings::default());
        assert_eq!(engine.current_erg_target, WARM_UP_TARGET_W);
    }

    #[test]
    fn should_ask_the_trainer_for_nothing_new_while_paused() {
        let mut engine = make_engine();
        engine.erg_ramp_rate = 20;
        engine.start();
        engine.tick(LiveReadings::default());
        engine.pause();
        engine.current_erg_target = 100;
        engine.tick(LiveReadings::default());
        assert_eq!(
            engine.current_erg_target, 100,
            "a paused ride holds whatever the trainer was last given"
        );
    }

    #[test]
    fn should_record_a_data_point_only_while_the_ride_is_running() {
        let mut engine = make_engine();
        engine.start();
        engine.tick(LiveReadings::default());
        assert_eq!(engine.session.data_points.len(), 1);
        engine.pause();
        engine.tick(LiveReadings::default());
        assert_eq!(
            engine.session.data_points.len(),
            1,
            "a paused ride is not being ridden, and records nothing"
        );
    }
}
