//! Auto-pause: stop the ride when the rider stops pedalling, and start it again
//! when they come back.
//!
//! Pure bookkeeping fed one power reading a second by whichever ride page is
//! running. It decides; the page acts, through the same path as its Pause
//! button, so a ride paused this way drops the time from its duration and lets
//! the trainer go exactly as a pressed pause does.
//!
//! Only a pause this started is ever ended by pedalling. A rider who pressed
//! Pause meant it, and turning the cranks to get off the bike must not undo it.

use super::engine::is_pedalling;

/// Seconds without pedalling before a running ride pauses itself.
pub const AUTO_PAUSE_SECS: u32 = 10;

/// Seconds of pedalling in a row before an auto-paused ride carries on — the
/// same wait as the pre-start countdown, for the same reason: time to get the
/// legs turning before the clock and the targets come back.
pub const AUTO_RESUME_SECS: u32 = super::engine::PRE_START_SECS;

/// Where the ride stands, as the page sees it this second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RideState {
    /// Not started yet, or already over. Auto-pause has nothing to do.
    NotRiding,
    Running,
    /// Paused for any reason — by the rider or by auto-pause.
    Paused,
}

/// What the page should do this second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Pause,
    Resume,
}

#[derive(Debug, Clone, Default)]
pub struct AutoPause {
    /// Seconds in a row without pedalling while running.
    stopped_for: u32,
    /// Seconds in a row of pedalling while auto-paused.
    pedalling_for: u32,
    /// Whether the pause in force is one this started.
    auto_paused: bool,
}

impl AutoPause {
    /// Feed one second.
    ///
    /// `enabled` gates only new pauses. A ride already auto-paused when the
    /// setting is switched off still resumes on pedalling — otherwise it would
    /// be left waiting for a button the rider has no reason to press.
    pub fn tick(&mut self, state: RideState, power_watts: Option<u32>, enabled: bool) -> Action {
        let pedalling = is_pedalling(power_watts);
        match state {
            RideState::NotRiding => {
                *self = Self::default();
                Action::None
            }
            RideState::Running => {
                // Running again, however it happened — a resume press while
                // auto-paused included — so no pause of ours is in force.
                self.auto_paused = false;
                self.pedalling_for = 0;
                if pedalling || !enabled {
                    self.stopped_for = 0;
                    return Action::None;
                }
                self.stopped_for += 1;
                if self.stopped_for >= AUTO_PAUSE_SECS {
                    self.stopped_for = 0;
                    self.auto_paused = true;
                    return Action::Pause;
                }
                Action::None
            }
            RideState::Paused => {
                self.stopped_for = 0;
                if !self.auto_paused {
                    return Action::None;
                }
                if !pedalling {
                    self.pedalling_for = 0;
                    return Action::None;
                }
                self.pedalling_for += 1;
                if self.pedalling_for >= AUTO_RESUME_SECS {
                    self.pedalling_for = 0;
                    self.auto_paused = false;
                    return Action::Resume;
                }
                Action::None
            }
        }
    }

    /// Whether the ride is paused because of auto-pause rather than the rider.
    pub fn is_auto_paused(&self) -> bool {
        self.auto_paused
    }

    /// Seconds of pedalling counted towards resuming, for the countdown.
    /// Zero while the rider is not pedalling.
    pub fn pedalling_for(&self) -> u32 {
        self.pedalling_for
    }
}

/// The banner title while auto-paused.
pub fn banner_title(pedalling_for: u32) -> String {
    if pedalling_for == 0 {
        "Paused — start pedalling to carry on".to_string()
    } else {
        format!(
            "Resuming in {} s…",
            AUTO_RESUME_SECS.saturating_sub(pedalling_for)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOPPED: Option<u32> = Some(0);
    const PEDALLING: Option<u32> = Some(180);

    /// Feed `n` seconds of the same reading and return the last action.
    fn feed(ap: &mut AutoPause, state: RideState, power: Option<u32>, n: u32) -> Action {
        let mut last = Action::None;
        for _ in 0..n {
            last = ap.tick(state, power, true);
        }
        last
    }

    // ── Pausing ─────────────────────────────────────────────────────────────

    #[test]
    fn should_not_pause_after_nine_seconds_stopped() {
        let mut ap = AutoPause::default();
        assert_eq!(feed(&mut ap, RideState::Running, STOPPED, 9), Action::None);
    }

    #[test]
    fn should_pause_on_exactly_the_tenth_second_stopped() {
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::Running, STOPPED, 9);
        assert_eq!(ap.tick(RideState::Running, STOPPED, true), Action::Pause);
        assert!(ap.is_auto_paused());
    }

    #[test]
    fn should_count_a_dropped_power_reading_as_stopped() {
        // No reading is not pedalling (see `is_pedalling`), so a rider whose
        // trainer has gone quiet is treated as stopped.
        let mut ap = AutoPause::default();
        assert_eq!(feed(&mut ap, RideState::Running, None, 10), Action::Pause);
    }

    #[test]
    fn should_start_the_count_again_after_one_pedal_stroke() {
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::Running, STOPPED, 9);
        ap.tick(RideState::Running, Some(1), true);
        // Nine more stopped seconds: nineteen in all, but never ten in a row.
        assert_eq!(feed(&mut ap, RideState::Running, STOPPED, 9), Action::None);
    }

    #[test]
    fn should_not_pause_when_switched_off() {
        let mut ap = AutoPause::default();
        for _ in 0..60 {
            assert_eq!(ap.tick(RideState::Running, STOPPED, false), Action::None);
        }
    }

    #[test]
    fn should_not_carry_stopped_seconds_across_switching_it_off() {
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::Running, STOPPED, 9);
        ap.tick(RideState::Running, STOPPED, false);
        assert_eq!(ap.tick(RideState::Running, STOPPED, true), Action::None);
    }

    #[test]
    fn should_not_pause_a_ride_that_has_not_started() {
        let mut ap = AutoPause::default();
        assert_eq!(
            feed(&mut ap, RideState::NotRiding, STOPPED, 60),
            Action::None
        );
    }

    #[test]
    fn should_not_carry_stopped_seconds_from_before_the_start() {
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::NotRiding, STOPPED, 9);
        assert_eq!(ap.tick(RideState::Running, STOPPED, true), Action::None);
    }

    #[test]
    fn should_pause_only_once_while_the_page_catches_up() {
        // The page may still report Running on the tick after a Pause.
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::Running, STOPPED, 10);
        assert_eq!(ap.tick(RideState::Running, STOPPED, true), Action::None);
    }

    // ── Resuming ────────────────────────────────────────────────────────────

    fn auto_paused() -> AutoPause {
        let mut ap = AutoPause::default();
        assert_eq!(
            feed(&mut ap, RideState::Running, STOPPED, 10),
            Action::Pause
        );
        ap
    }

    #[test]
    fn should_not_resume_after_nine_seconds_pedalling() {
        let mut ap = auto_paused();
        assert_eq!(feed(&mut ap, RideState::Paused, PEDALLING, 9), Action::None);
        assert_eq!(ap.pedalling_for(), 9);
    }

    #[test]
    fn should_resume_on_exactly_the_tenth_second_pedalling() {
        let mut ap = auto_paused();
        feed(&mut ap, RideState::Paused, PEDALLING, 9);
        assert_eq!(ap.tick(RideState::Paused, PEDALLING, true), Action::Resume);
        assert!(!ap.is_auto_paused());
        assert_eq!(ap.pedalling_for(), 0);
    }

    #[test]
    fn should_restart_the_resume_count_when_the_rider_stops_again() {
        let mut ap = auto_paused();
        feed(&mut ap, RideState::Paused, PEDALLING, 9);
        ap.tick(RideState::Paused, STOPPED, true);
        assert_eq!(ap.pedalling_for(), 0);
        assert_eq!(feed(&mut ap, RideState::Paused, PEDALLING, 9), Action::None);
    }

    #[test]
    fn should_stay_paused_while_the_rider_stays_off() {
        let mut ap = auto_paused();
        assert_eq!(feed(&mut ap, RideState::Paused, STOPPED, 600), Action::None);
        assert!(ap.is_auto_paused());
    }

    #[test]
    fn should_never_end_a_pause_the_rider_pressed() {
        let mut ap = AutoPause::default();
        feed(&mut ap, RideState::Running, PEDALLING, 30);
        assert_eq!(
            feed(&mut ap, RideState::Paused, PEDALLING, 600),
            Action::None
        );
        assert!(!ap.is_auto_paused());
    }

    #[test]
    fn should_forget_its_pause_when_the_rider_resumes_by_hand() {
        let mut ap = auto_paused();
        ap.tick(RideState::Running, PEDALLING, true);
        assert!(!ap.is_auto_paused());
        // Then the rider pauses by hand: that pause is theirs.
        assert_eq!(
            feed(&mut ap, RideState::Paused, PEDALLING, 60),
            Action::None
        );
    }

    #[test]
    fn should_still_resume_when_switched_off_while_auto_paused() {
        let mut ap = auto_paused();
        for _ in 0..9 {
            assert_eq!(ap.tick(RideState::Paused, PEDALLING, false), Action::None);
        }
        assert_eq!(ap.tick(RideState::Paused, PEDALLING, false), Action::Resume);
    }

    #[test]
    fn should_forget_everything_when_the_ride_ends() {
        let mut ap = auto_paused();
        ap.tick(RideState::NotRiding, STOPPED, true);
        assert!(!ap.is_auto_paused());
        assert_eq!(ap.pedalling_for(), 0);
    }

    #[test]
    fn should_pause_again_ten_seconds_after_an_auto_resume() {
        let mut ap = auto_paused();
        feed(&mut ap, RideState::Paused, PEDALLING, 10);
        assert_eq!(
            feed(&mut ap, RideState::Running, STOPPED, 10),
            Action::Pause
        );
    }

    // ── The banner ──────────────────────────────────────────────────────────

    #[test]
    fn should_ask_for_pedalling_before_any_has_been_counted() {
        assert_eq!(banner_title(0), "Paused — start pedalling to carry on");
    }

    #[test]
    fn should_count_down_from_nine_after_the_first_second() {
        assert_eq!(banner_title(1), "Resuming in 9 s…");
        assert_eq!(banner_title(9), "Resuming in 1 s…");
    }
}
