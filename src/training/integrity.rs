//! Whether a recorded ride's numbers describe what the rider actually did.
//!
//! Everything downstream of a ride — its Training Stress Score, the Fitness and
//! Fatigue averages built from it, the training-load figure written into the FIT
//! file, the summary handed to the coach — is derived from the same two things:
//! the power samples, and the wall-clock span the ride covered. Nothing between
//! "recorded" and "counted" has ever asked whether those two agree.
//!
//! They come apart in one specific way, and it always overstates the ride.
//! [`Session::normalised_power`] averages the samples that carry power and
//! silently skips the ones that do not, while [`Session::duration_secs`] is
//! start to finish on the clock. A trainer that drops off the air for half an
//! hour therefore contributes the *intensity of the half that recorded* spread
//! over the *whole* elapsed time — a confident, plausible, and completely wrong
//! number that no one looking at the ride would have any reason to doubt.
//!
//! [`crate::training::load`] already refuses a heart-rate trace that is too flat
//! or too sparse to integrate, rather than reporting a wrong load from it. This
//! is the same judgement raised to the whole ride.
//!
//! **Only faults that make the ride's own numbers wrong belong here.** A dud
//! heart-rate strap is not one of them: the load estimator already falls back to
//! power on its own, and TSS never looked at heart rate to begin with. Flagging
//! a ride for it would hold back a perfectly good power trace.

use serde::{Deserialize, Serialize};

use crate::data::session::Session;
use crate::training::load::sample_secs;

/// Below this fraction of the ride carrying power, the numbers derived from
/// power are describing a different ride from the one on the clock.
///
/// Not a tight bound: a few seconds lost to a reconnect are normal and shift
/// the answer by nothing. A fifth of the ride missing is not normal.
const MIN_POWER_COVERAGE: f32 = 0.8;

/// A whole ride's power varying by less than this is a number being repeated,
/// not a rider holding an effort. Even ERG mode at a fixed target wanders
/// further than this across a warmup, the work and a cooldown.
const MIN_POWER_SPREAD_WATTS: u32 = 5;

/// Power no human produces, so a reading at or above it came from the wire
/// rather than the pedals. Matches the sanitising ceiling in CLAUDE.md §5.1.
const IMPLAUSIBLE_POWER_WATTS: u32 = 3000;

/// Rides shorter than this are not assessed.
///
/// Every check below is a ratio or a spread measured across a whole ride, and
/// across a couple of minutes those are noise: one reconnect is a third of the
/// coverage, and a steady two-minute effort really can hold five watts. Two
/// minutes at threshold is three TSS, so nothing that matters is waved through.
const MIN_ASSESSABLE_SECS: u64 = 120;

/// One reason a ride's numbers cannot be taken at face value.
///
/// Each carries the figures behind it so the UI can say what was actually seen
/// rather than that something, somewhere, looked wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Concern {
    /// Power was recorded for `covered_secs` of a `ride_secs` ride. The gap is
    /// scored at the intensity of the part that did record, which is what makes
    /// this the concern that matters most.
    PowerCoverage { covered_secs: u32, ride_secs: u32 },
    /// Every power reading in the ride is the same value: a trainer repeating a
    /// number rather than measuring one.
    StuckPower { watts: u32 },
    /// A reading beyond anything a rider can produce.
    ImplausiblePower { max_watts: u32 },
}

impl Concern {
    /// A single sentence naming what was seen, for the rider rather than the log.
    pub fn describe(&self) -> String {
        match *self {
            Concern::PowerCoverage {
                covered_secs,
                ride_secs,
            } => {
                let missing = ride_secs.saturating_sub(covered_secs);
                format!(
                    "Power stopped recording for {} of this {} ride.",
                    format_span(missing),
                    format_span(ride_secs)
                )
            }
            Concern::StuckPower { watts } => {
                format!("Power read exactly {watts} W for the whole ride.")
            }
            Concern::ImplausiblePower { max_watts } => {
                format!("A power reading of {max_watts} W came through, which no rider produces.")
            }
        }
    }

    /// What the rider can do about it, if anything.
    pub fn remedy(&self) -> &'static str {
        match self {
            Concern::PowerCoverage { .. } => {
                "Check the trainer's connection, or its batteries if it has any."
            }
            Concern::StuckPower { .. } => {
                "The trainer was reporting a fixed value rather than measuring one."
            }
            Concern::ImplausiblePower { .. } => {
                "A stray reading like this usually means interference, not a fault."
            }
        }
    }
}

/// Round a span of seconds to the coarsest unit that still says something:
/// whole minutes above a minute, seconds below it.
fn format_span(secs: u32) -> String {
    match secs {
        0..=59 => format!("{secs} sec"),
        _ => {
            let mins = (secs + 30) / 60;
            format!("{mins} min")
        }
    }
}

/// Everything wrong with a ride, or nothing.
///
/// Serialises as a plain array so the stored form is the concern list itself,
/// with no wrapper object to version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Verdict(Vec<Concern>);

impl Verdict {
    /// A ride whose numbers can be counted as they stand.
    pub fn is_trusted(&self) -> bool {
        self.0.is_empty()
    }

    /// Everything found, in the order it was checked.
    pub fn concerns(&self) -> &[Concern] {
        &self.0
    }

    /// The stored form: a JSON array, `[]` for a ride with nothing wrong.
    pub fn to_json(&self) -> String {
        // A list of plain enums with integer fields cannot fail to serialise,
        // and a ride is not worth refusing to save over a formatting error.
        serde_json::to_string(self).unwrap_or_else(|e| {
            tracing::error!("could not serialise ride integrity ({e}) — recording it as trusted");
            "[]".to_string()
        })
    }

    /// Read back what [`Verdict::to_json`] wrote.
    ///
    /// An unreadable value is treated as trusted rather than as a fault: the
    /// concern list is a cached judgement, and a ride must not be held back from
    /// the rider's own history because the cache of *why* went bad.
    pub fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_else(|e| {
            tracing::warn!("ride integrity {raw:?} is unreadable ({e}) — treating it as trusted");
            Verdict::default()
        })
    }
}

/// Seconds of the ride that carry a power reading, each sample standing for the
/// gap to the next one.
///
/// Gap-weighted rather than counted, because the two failures this has to catch
/// look different in the samples: a trainer that drops off the air leaves points
/// with no power in them, while an app left running after the ride simply stops
/// producing points at all. Both are elapsed time that no power describes.
fn covered_secs(session: &Session) -> u32 {
    let points = &session.data_points;
    points
        .iter()
        .enumerate()
        .filter(|(_, p)| p.power_watts.is_some())
        .map(|(i, _)| sample_secs(points, i))
        .sum::<f32>()
        .round() as u32
}

/// Assess a finished ride.
///
/// Returns a trusted verdict for a ride carrying no power at all: there is
/// nothing to overstate, [`Session::tss`] already declines to score it, and
/// telling the rider that a ride they rode without a trainer connected is
/// untrustworthy would be noise rather than news.
pub fn check(session: &Session) -> Verdict {
    let ride_secs = session.duration_secs();
    if ride_secs < MIN_ASSESSABLE_SECS {
        return Verdict::default();
    }

    let powers: Vec<u32> = session
        .data_points
        .iter()
        .filter_map(|p| p.power_watts)
        .collect();
    if powers.is_empty() {
        return Verdict::default();
    }

    let mut concerns = Vec::new();

    let covered = covered_secs(session);
    if (covered as f32) < MIN_POWER_COVERAGE * ride_secs as f32 {
        concerns.push(Concern::PowerCoverage {
            covered_secs: covered,
            // A ride is capped at the u32 seconds the rest of the app measures
            // it in; nothing recorded here approaches 136 years.
            ride_secs: ride_secs.min(u32::MAX as u64) as u32,
        });
    }

    // Both ends exist: the vector is non-empty.
    let (min, max) = (
        powers.iter().copied().min().unwrap_or(0),
        powers.iter().copied().max().unwrap_or(0),
    );
    if max - min < MIN_POWER_SPREAD_WATTS {
        concerns.push(Concern::StuckPower { watts: min });
    }
    if max >= IMPLAUSIBLE_POWER_WATTS {
        concerns.push(Concern::ImplausiblePower { max_watts: max });
    }

    Verdict(concerns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::DataPoint;

    /// A ride `ride_secs` long on the clock, carrying a 1 Hz sample for each of
    /// the first `recorded_secs`.
    ///
    /// Power cycles through a 60 W range so that no ride built here reads as a
    /// trainer stuck on one value — that is a separate concern with its own
    /// tests, and it would otherwise fire on every case below.
    fn ride(ride_secs: u64, recorded_secs: u32) -> Session {
        let mut session = Session::new(None);
        session.ended_at = Some(session.started_at + chrono::Duration::seconds(ride_secs as i64));
        session.data_points = (0..recorded_secs)
            .map(|i| DataPoint {
                elapsed_secs: i,
                power_watts: Some(150 + i % 60),
                target_watts: None,
                heart_rate_bpm: Some(140),
                cadence_rpm: Some(90),
                speed_kmh: Some(30.0),
                lat: None,
                lng: None,
                altitude_m: None,
            })
            .collect();
        session
    }

    /// Drop the power reading from every sample at or after `from_secs`, the way
    /// a trainer going quiet mid-ride does while the recorder keeps writing.
    fn lose_power_from(session: &mut Session, from_secs: u32) {
        for p in session.data_points.iter_mut().skip(from_secs as usize) {
            p.power_watts = None;
        }
    }

    fn concerns(session: &Session) -> Vec<Concern> {
        check(session).concerns().to_vec()
    }

    #[test]
    fn should_trust_a_ride_that_recorded_all_the_way_through() {
        assert!(check(&ride(3600, 3600)).is_trusted());
    }

    #[test]
    fn should_flag_a_ride_whose_trainer_went_quiet_midway() {
        // The failure this exists for: normalised power averages the half that
        // recorded, duration counts the whole hour, and the TSS that comes out
        // is the intensity of one half spread over both.
        let mut session = ride(3600, 3600);
        lose_power_from(&mut session, 1800);
        assert_eq!(
            concerns(&session),
            vec![Concern::PowerCoverage {
                covered_secs: 1800,
                ride_secs: 3600
            }]
        );
    }

    #[test]
    fn should_flag_a_ride_the_app_was_left_running_after() {
        // Ten minutes ridden, an hour on the clock. No samples are missing
        // power here — the samples themselves stop, which is why coverage is
        // measured against elapsed time rather than against the sample count.
        let session = ride(3600, 600);
        assert_eq!(
            concerns(&session),
            vec![Concern::PowerCoverage {
                covered_secs: 600,
                ride_secs: 3600
            }]
        );
    }

    #[test]
    fn should_trust_a_ride_covered_to_exactly_the_threshold() {
        // 480 of 600 seconds is 80 % on the nose, and the bound is inclusive.
        let mut session = ride(600, 600);
        lose_power_from(&mut session, 480);
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_flag_a_ride_one_second_under_the_threshold() {
        let mut session = ride(600, 600);
        lose_power_from(&mut session, 479);
        assert_eq!(
            concerns(&session),
            vec![Concern::PowerCoverage {
                covered_secs: 479,
                ride_secs: 600
            }]
        );
    }

    #[test]
    fn should_count_a_sample_for_the_gap_to_the_next_one() {
        // Recorded every five seconds for ten minutes: 120 samples covering the
        // full 600 seconds, not 120 of them.
        let mut session = ride(600, 120);
        for (i, p) in session.data_points.iter_mut().enumerate() {
            p.elapsed_secs = i as u32 * 5;
        }
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_flag_a_trainer_repeating_one_value() {
        let mut session = ride(600, 600);
        for p in &mut session.data_points {
            p.power_watts = Some(200);
        }
        assert_eq!(concerns(&session), vec![Concern::StuckPower { watts: 200 }]);
    }

    #[test]
    fn should_flag_a_trainer_reporting_nothing_but_zero() {
        // Zero watts all ride is the same fault wearing a more plausible face:
        // it scores as a ride that happened and cost nothing.
        let mut session = ride(600, 600);
        for p in &mut session.data_points {
            p.power_watts = Some(0);
        }
        assert_eq!(concerns(&session), vec![Concern::StuckPower { watts: 0 }]);
    }

    #[test]
    fn should_trust_a_ride_that_varied_by_exactly_the_minimum_spread() {
        let mut session = ride(600, 600);
        for (i, p) in session.data_points.iter_mut().enumerate() {
            p.power_watts = Some(200 + (i as u32 % 2) * MIN_POWER_SPREAD_WATTS);
        }
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_flag_a_ride_that_varied_by_one_watt_less() {
        let mut session = ride(600, 600);
        for (i, p) in session.data_points.iter_mut().enumerate() {
            p.power_watts = Some(200 + (i as u32 % 2) * (MIN_POWER_SPREAD_WATTS - 1));
        }
        assert_eq!(concerns(&session), vec![Concern::StuckPower { watts: 200 }]);
    }

    #[test]
    fn should_flag_a_reading_no_rider_produces() {
        let mut session = ride(600, 600);
        session.data_points[42].power_watts = Some(IMPLAUSIBLE_POWER_WATTS);
        assert_eq!(
            concerns(&session),
            vec![Concern::ImplausiblePower {
                max_watts: IMPLAUSIBLE_POWER_WATTS
            }]
        );
    }

    #[test]
    fn should_trust_a_reading_one_watt_under_the_impossible() {
        let mut session = ride(600, 600);
        session.data_points[42].power_watts = Some(IMPLAUSIBLE_POWER_WATTS - 1);
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_report_every_concern_a_ride_has() {
        // A stray reading and a dropout are independent faults, and a ride can
        // have both — the rider should be told about both.
        let mut session = ride(3600, 3600);
        lose_power_from(&mut session, 1000);
        session.data_points[42].power_watts = Some(9999);
        assert_eq!(
            concerns(&session),
            vec![
                Concern::PowerCoverage {
                    covered_secs: 1000,
                    ride_secs: 3600
                },
                Concern::ImplausiblePower { max_watts: 9999 },
            ]
        );
    }

    #[test]
    fn should_not_flag_a_ride_that_recorded_no_power_at_all() {
        // A ride with the trainer never connected has nothing to overstate:
        // TSS already declines to score it, and it contributes nothing.
        let mut session = ride(3600, 3600);
        lose_power_from(&mut session, 0);
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_not_flag_an_empty_ride() {
        assert!(check(&Session::new(None)).is_trusted());
    }

    #[test]
    fn should_not_assess_a_ride_shorter_than_the_minimum() {
        // One second under the bound, and badly broken: no power for any of it
        // and a stray reading besides.
        let mut session = ride(MIN_ASSESSABLE_SECS - 1, 10);
        session.data_points[0].power_watts = Some(9999);
        assert!(check(&session).is_trusted());
    }

    #[test]
    fn should_assess_a_ride_of_exactly_the_minimum() {
        let session = ride(MIN_ASSESSABLE_SECS, 10);
        assert_eq!(
            concerns(&session),
            vec![Concern::PowerCoverage {
                covered_secs: 10,
                ride_secs: MIN_ASSESSABLE_SECS as u32
            }]
        );
    }

    // ── The stored form ─────────────────────────────────────────────────────

    #[test]
    fn should_survive_a_round_trip_through_the_stored_form() {
        let mut session = ride(3600, 3600);
        lose_power_from(&mut session, 1800);
        let verdict = check(&session);
        assert_eq!(Verdict::from_json(&verdict.to_json()), verdict);
    }

    #[test]
    fn should_store_a_trusted_ride_as_an_empty_list() {
        assert_eq!(Verdict::default().to_json(), "[]");
        assert!(Verdict::from_json("[]").is_trusted());
    }

    #[test]
    fn should_read_an_unreadable_stored_verdict_as_trusted() {
        // The concern list is a cached judgement. A ride must not be held back
        // from the rider's own history because the cache of why went bad.
        assert!(Verdict::from_json("{ this is not json").is_trusted());
        assert!(Verdict::from_json("").is_trusted());
    }

    // ── What the rider is told ──────────────────────────────────────────────

    #[test]
    fn should_say_how_much_of_the_ride_went_missing() {
        let text = Concern::PowerCoverage {
            covered_secs: 1620,
            ride_secs: 3660,
        }
        .describe();
        assert_eq!(
            text,
            "Power stopped recording for 34 min of this 61 min ride."
        );
    }

    #[test]
    fn should_name_a_span_in_the_coarsest_unit_that_still_says_something() {
        assert_eq!(format_span(0), "0 sec");
        assert_eq!(format_span(59), "59 sec");
        assert_eq!(format_span(60), "1 min");
        assert_eq!(format_span(89), "1 min");
        assert_eq!(format_span(90), "2 min");
    }

    #[test]
    fn should_describe_every_concern_without_naming_a_bare_number() {
        // Each description has to stand alone in a banner, so none of them may
        // read as a fragment or leave a placeholder unfilled.
        for concern in [
            Concern::PowerCoverage {
                covered_secs: 10,
                ride_secs: 600,
            },
            Concern::StuckPower { watts: 200 },
            Concern::ImplausiblePower { max_watts: 9999 },
        ] {
            let text = concern.describe();
            assert!(text.ends_with('.'), "{text}");
            assert!(!text.contains("{"), "{text}");
            assert!(!concern.remedy().is_empty());
        }
    }
}
