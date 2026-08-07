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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
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
    pub target_power_watts: u32,
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
    current_erg_target: u32,
    /// Set by `resume_session` until the ride actually restarts. Keeps `start`
    /// from re-stamping `started_at` over the original ride's start time.
    resuming: bool,
    /// Wall-clock time the ride was not being ridden for because the app was
    /// closed. Subtracted at `stop` so the gap does not inflate the ride's
    /// duration, and through it the TSS.
    interruption: chrono::Duration,
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
        Self {
            session,
            workout,
            athlete,
            state: EngineState::Idle,
            start_instant: None,
            pause_offset: Duration::ZERO,
            device_cmd_tx,
            erg_ramp_rate: 25,
            current_erg_target: 0,
            resuming: false,
            interruption: chrono::Duration::zero(),
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
        self.workout = workout;
        self.session = session;
        self.state = EngineState::Idle;
        self.start_instant = None;
        self.pause_offset = Duration::from_secs(elapsed as u64);
        self.current_erg_target = 0;
        self.resuming = true;
        self.interruption = chrono::Duration::zero();
    }

    pub fn pause(&mut self) {
        if self.state == EngineState::Running {
            if let Some(start) = self.start_instant {
                self.pause_offset += start.elapsed();
                self.start_instant = None;
            }
            self.state = EngineState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.state == EngineState::Paused {
            self.start_instant = Some(Instant::now());
            self.state = EngineState::Running;
        }
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
        self.workout = workout;
        self.session = Session::new(workout_id);
        self.session.ftp_watts = Some(self.athlete.borrow().ftp_watts);
        self.state = EngineState::Idle;
        self.start_instant = None;
        self.pause_offset = Duration::ZERO;
        self.current_erg_target = 0;
        self.resuming = false;
        self.interruption = chrono::Duration::zero();
    }

    pub fn stop(&mut self) {
        self.state = EngineState::Completed;
        // Discount any time the ride spent interrupted, so a session picked back
        // up an hour later is not recorded as an hour longer than it was ridden.
        self.session.ended_at = Some(chrono::Utc::now() - self.interruption);
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

        // `planned_target` is None once past the last segment — recorded data
        // points distinguish "no target" from "target of 0 W" for FTP detection.
        let (target_watts, seg_remaining, planned_target) =
            if let Some(seg) = self.workout.segments.get(seg_idx) {
                let ftp = self.athlete.borrow().ftp_watts;
                let target = seg.target_power_at(seg_elapsed, ftp);
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
                target_watts: planned_target,
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
}
