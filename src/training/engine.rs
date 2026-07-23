use crate::data::{
    athlete::AthleteProfile,
    session::{DataPoint, LiveReadings, Session},
    workout::Workout, // Segment removed — was unused
};
use crate::devices::manager::DeviceCommand;
use async_channel::Sender;
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
    pub athlete: AthleteProfile,
    pub session: Session,
    pub state: EngineState,
    start_instant: Option<Instant>,
    pause_offset: Duration,
    device_cmd_tx: Sender<DeviceCommand>,
    /// Max watts per second the ERG target may change. 0 = instant (no smoothing).
    pub erg_ramp_rate: u32,
    current_erg_target: u32,
}

impl WorkoutEngine {
    pub fn new(
        workout: Workout,
        athlete: AthleteProfile,
        device_cmd_tx: Sender<DeviceCommand>,
    ) -> Self {
        let mut session = Session::new(Some(workout.id));
        // Stamp the FTP the ride is executed at — FTP detection needs it to
        // interpret targets after the profile FTP changes.
        session.ftp_watts = Some(athlete.ftp_watts);
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
        }
    }

    pub fn start(&mut self) {
        self.start_instant = Some(Instant::now());
        self.state = EngineState::Running;
        tracing::info!("Workout started: {}", self.workout.name);
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
        self.session.ftp_watts = Some(self.athlete.ftp_watts);
        self.state = EngineState::Idle;
        self.start_instant = None;
        self.pause_offset = Duration::ZERO;
        self.current_erg_target = 0;
    }

    pub fn stop(&mut self) {
        self.state = EngineState::Completed;
        self.session.ended_at = Some(chrono::Utc::now());
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
                let target = seg.target_power_at(seg_elapsed, self.athlete.ftp_watts);
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
