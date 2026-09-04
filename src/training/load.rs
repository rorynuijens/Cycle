//! Training-load and Training Effect estimation for exported activities.
//!
//! Garmin Connect does not compute training load for an activity it did not
//! record — it reads the finished numbers out of the FIT file (session fields
//! 24, 137 and 168). A file without them shows a ride on the calendar but
//! contributes nothing to training status, which is why the app has to work
//! these out itself.
//!
//! The aerobic figure is a Banister TRIMP: each second is weighted by the
//! rider's heart-rate reserve through an exponential, so hard minutes count for
//! much more than easy ones. Checked against two rides the rider recorded on a
//! Garmin Edge, TRIMP reproduced the head unit's own training load to within
//! 1.5 % on a two-hour ride and under-reported a 19-minute hard ride — short
//! spiky efforts are where Firstbeat's EPOC model and a TRIMP part company.
//!
//! Rides without a usable heart-rate trace fall back to power. Smoothed power
//! is mapped onto the heart-rate reserve the rider would have been holding,
//! which lands within about 2 % of the heart-rate answer on both reference
//! rides — good enough to be worth exporting, and clearly marked as an estimate.

use crate::data::athlete::AthleteProfile;
use crate::data::session::Session;

/// Which signal the estimate was derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSource {
    /// Measured heart rate — the signal Garmin's own model uses.
    HeartRate,
    /// Power, mapped onto an equivalent heart-rate reserve.
    Power,
}

/// An activity's training load, in the units Garmin Connect expects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainingLoad {
    /// Training load — peak EPOC in ml/kg, the number Garmin sums over 7 days.
    pub load: f32,
    /// Aerobic Training Effect, 0.0–5.0.
    pub aerobic_te: f32,
    /// Anaerobic Training Effect, 0.0–5.0.
    pub anaerobic_te: f32,
    pub source: LoadSource,
}

/// A heart-rate trace this flat is a sensor reporting a fixed value rather than
/// a rider's heart, and must not be fed to the aerobic model.
const MIN_HR_SPREAD_BPM: u32 = 10;

/// Below this coverage the heart-rate trace has too many holes to integrate.
const MIN_HR_COVERAGE: f32 = 0.5;

/// Heart rate lags power by roughly this time constant, so power is smoothed by
/// the same amount before it stands in for heart rate.
const POWER_SMOOTHING_SECS: f32 = 90.0;

/// Heart-rate reserve held at zero watts — freewheeling does not drop a riding
/// heart rate to resting. Fitted with [`HR_RESERVE_PER_FTP`] against the
/// rider's own Garmin rides.
const HR_RESERVE_AT_ZERO: f32 = 0.22;

/// Extra heart-rate reserve per unit of FTP-relative power.
const HR_RESERVE_PER_FTP: f32 = 0.70;

/// Power above this fraction of FTP contributes to the anaerobic estimate.
const ANAEROBIC_THRESHOLD_FRAC: f32 = 1.05;

/// Banister TRIMP weighting for one second at a given heart-rate reserve.
fn trimp_increment(hr_reserve: f32, secs: f32) -> f32 {
    let r = hr_reserve.clamp(0.0, 1.0);
    (secs / 60.0) * r * 0.64 * (1.92 * r).exp()
}

/// Aerobic Training Effect for a training load, on Firstbeat's 0–5 scale.
///
/// Logarithmic, fitted to two Garmin-recorded rides by the same athlete
/// (load 202 → TE 4.4, load 48 → TE 2.3).
fn aerobic_te_for_load(load: f32) -> f32 {
    if load <= 1.0 {
        return 0.0;
    }
    (1.458 * load.ln() - 3.34).clamp(0.0, 5.0)
}

/// Seconds between one data point and the next, defaulting to the 1 Hz the
/// recorder writes when the gap cannot be read (last point, or a clock jump).
fn sample_secs(points: &[crate::data::session::DataPoint], i: usize) -> f32 {
    match points.get(i + 1) {
        Some(next) => {
            (next.elapsed_secs.saturating_sub(points[i].elapsed_secs)).clamp(1, 60) as f32
        }
        None => 1.0,
    }
}

/// Whether the heart-rate trace can carry the aerobic estimate.
///
/// Rejects both a sparse trace and a flat one: a strap that dropped out and
/// reported a single fixed value all ride would otherwise produce a confident
/// and completely wrong load.
fn hr_is_usable(session: &Session) -> bool {
    let hrs: Vec<u32> = session
        .data_points
        .iter()
        .filter_map(|p| p.heart_rate_bpm)
        .filter(|&h| h > 0)
        .collect();
    if session.data_points.is_empty() || hrs.is_empty() {
        return false;
    }
    let coverage = hrs.len() as f32 / session.data_points.len() as f32;
    // Both ends exist: the vector is non-empty.
    let spread = hrs.iter().max().copied().unwrap_or(0) - hrs.iter().min().copied().unwrap_or(0);
    coverage >= MIN_HR_COVERAGE && spread >= MIN_HR_SPREAD_BPM
}

/// The anaerobic Training Effect, from time spent above threshold.
///
/// Fitted to the same two reference rides (proxy 170 → TE 2.2, proxy 35 → 1.4).
fn anaerobic_te(session: &Session, ftp: u32) -> f32 {
    if ftp == 0 {
        return 0.0;
    }
    let mut proxy = 0.0f32;
    for (i, p) in session.data_points.iter().enumerate() {
        let Some(watts) = p.power_watts else { continue };
        let frac = watts as f32 / ftp as f32;
        if frac > ANAEROBIC_THRESHOLD_FRAC {
            let excess = frac - ANAEROBIC_THRESHOLD_FRAC;
            proxy += excess * excess * sample_secs(&session.data_points, i);
        }
    }
    if proxy <= 1.0 {
        return 0.0;
    }
    (0.506 * proxy.ln() - 0.40).clamp(0.0, 5.0)
}

/// Estimate the training load of a finished ride.
///
/// Returns `None` for a ride carrying neither a usable heart-rate trace nor
/// power — there is nothing to derive a load from, and a fabricated zero would
/// be worse than leaving the field out of the export.
pub fn estimate(session: &Session, athlete: &AthleteProfile) -> Option<TrainingLoad> {
    let ftp = session.ftp_watts.unwrap_or(athlete.ftp_watts);
    let points = &session.data_points;
    if points.is_empty() {
        return None;
    }

    let (load, source) = if hr_is_usable(session) {
        // The reserve span cannot be zero or negative, whatever the profile says.
        let span = (athlete.max_hr.saturating_sub(athlete.resting_hr)).max(1) as f32;
        let mut total = 0.0;
        for (i, p) in points.iter().enumerate() {
            let Some(hr) = p.heart_rate_bpm else { continue };
            let reserve = (hr as f32 - athlete.resting_hr as f32) / span;
            total += trimp_increment(reserve, sample_secs(points, i));
        }
        (total, LoadSource::HeartRate)
    } else if ftp > 0 && points.iter().any(|p| p.power_watts.is_some()) {
        let mut smoothed: Option<f32> = None;
        let mut total = 0.0;
        for (i, p) in points.iter().enumerate() {
            let Some(watts) = p.power_watts else { continue };
            let secs = sample_secs(points, i);
            let w = watts as f32;
            smoothed = Some(match smoothed {
                None => w,
                Some(prev) => prev + (w - prev) * (secs / POWER_SMOOTHING_SECS),
            });
            let frac = smoothed.unwrap_or(w) / ftp as f32;
            let reserve = HR_RESERVE_AT_ZERO + HR_RESERVE_PER_FTP * frac;
            total += trimp_increment(reserve, secs);
        }
        (total, LoadSource::Power)
    } else {
        return None;
    };

    // Garmin's own field is a 0–1000 scale; anything beyond that is a bug or a
    // rogue trace, not a ride.
    let load = load.clamp(0.0, 1000.0);
    Some(TrainingLoad {
        load,
        aerobic_te: aerobic_te_for_load(load),
        anaerobic_te: anaerobic_te(session, ftp),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::DataPoint;

    fn athlete() -> AthleteProfile {
        AthleteProfile {
            ftp_watts: 200,
            max_hr: 195,
            resting_hr: 52,
            ..AthleteProfile::default()
        }
    }

    /// A ride of `mins` minutes at fixed power and (optionally) fixed heart rate.
    fn ride(mins: u32, watts: Option<u32>, hr: Option<u32>) -> Session {
        let mut session = Session::new(None);
        session.ended_at = Some(session.started_at + chrono::Duration::minutes(mins as i64));
        for i in 0..mins * 60 {
            session.data_points.push(DataPoint {
                elapsed_secs: i,
                power_watts: watts,
                target_watts: None,
                // A flat trace is rejected, so nudge the heart rate each minute.
                heart_rate_bpm: hr.map(|h| h + (i / 60) % 12),
                cadence_rpm: Some(90),
                speed_kmh: Some(30.0),
                lat: None,
                lng: None,
                altitude_m: None,
            });
        }
        session
    }

    #[test]
    fn should_return_none_when_the_ride_has_no_data_at_all() {
        assert!(estimate(&Session::new(None), &athlete()).is_none());
    }

    #[test]
    fn should_return_none_when_there_is_neither_heart_rate_nor_power() {
        let session = ride(20, None, None);
        assert!(estimate(&session, &athlete()).is_none());
    }

    #[test]
    fn should_use_heart_rate_when_the_trace_is_usable() {
        let est = estimate(&ride(60, Some(200), Some(150)), &athlete()).expect("has data");
        assert_eq!(est.source, LoadSource::HeartRate);
    }

    #[test]
    fn should_fall_back_to_power_when_heart_rate_is_stuck_at_one_value() {
        // The failure this guards against: a strap that reported 94 bpm for a
        // whole ride, which would otherwise read as a long easy effort.
        let mut session = ride(60, Some(200), None);
        for p in &mut session.data_points {
            p.heart_rate_bpm = Some(94);
        }
        let est = estimate(&session, &athlete()).expect("has power");
        assert_eq!(est.source, LoadSource::Power);
    }

    #[test]
    fn should_fall_back_to_power_when_heart_rate_covers_too_little_of_the_ride() {
        let mut session = ride(60, Some(200), Some(150));
        for p in session.data_points.iter_mut().take(3000) {
            p.heart_rate_bpm = None;
        }
        let est = estimate(&session, &athlete()).expect("has power");
        assert_eq!(est.source, LoadSource::Power);
    }

    #[test]
    fn should_score_an_hour_at_threshold_near_a_hundred() {
        // An hour at FTP is 100 TSS by definition; Garmin's load runs a little
        // higher than TSS, so the band is wide but the order of magnitude is not.
        let est = estimate(&ride(60, Some(200), None), &athlete()).expect("has power");
        assert!((80.0..=220.0).contains(&est.load), "load was {}", est.load);
    }

    #[test]
    fn should_rate_a_long_hard_ride_above_a_short_easy_one() {
        let hard = estimate(&ride(120, Some(210), None), &athlete()).expect("has power");
        let easy = estimate(&ride(20, Some(110), None), &athlete()).expect("has power");
        assert!(hard.load > easy.load);
        assert!(hard.aerobic_te > easy.aerobic_te);
    }

    #[test]
    fn should_keep_training_effect_inside_the_firstbeat_scale() {
        let est = estimate(&ride(300, Some(260), None), &athlete()).expect("has power");
        assert!((0.0..=5.0).contains(&est.aerobic_te));
        assert!((0.0..=5.0).contains(&est.anaerobic_te));
        assert!(est.load <= 1000.0);
    }

    #[test]
    fn should_report_no_anaerobic_effect_for_a_ride_entirely_below_threshold() {
        let est = estimate(&ride(60, Some(120), None), &athlete()).expect("has power");
        assert_eq!(est.anaerobic_te, 0.0);
    }

    #[test]
    fn should_report_anaerobic_effect_for_repeated_efforts_above_threshold() {
        let est = estimate(&ride(30, Some(300), None), &athlete()).expect("has power");
        assert!(
            est.anaerobic_te > 1.0,
            "anaerobic TE was {}",
            est.anaerobic_te
        );
    }

    #[test]
    fn should_not_divide_by_zero_when_max_and_resting_heart_rate_match() {
        let profile = AthleteProfile {
            max_hr: 150,
            resting_hr: 150,
            ..athlete()
        };
        let est = estimate(&ride(30, Some(200), Some(140)), &profile).expect("has data");
        assert!(est.load.is_finite());
    }

    #[test]
    fn should_prefer_the_ride_time_ftp_over_the_current_profile_ftp() {
        // Same ride, but recorded when the rider's FTP was half what it is now:
        // the effort was relatively harder, so the load must come out higher.
        let mut then = ride(45, Some(200), None);
        then.ftp_watts = Some(100);
        let now = ride(45, Some(200), None);
        let a = athlete();
        assert!(
            estimate(&then, &a).expect("has power").load
                > estimate(&now, &a).expect("has power").load
        );
    }

    // ── The arithmetic itself ───────────────────────────────────────────────
    //
    // The tests above check direction and order of magnitude — a long hard ride
    // outscores a short easy one, an hour at threshold lands in an 80–220 band.
    // A band that wide tolerates almost any error inside it, and mutation
    // testing duly found every operator in the estimator swappable without a
    // failure. What follows pins values instead of ranges: small rides whose
    // answer can be worked out from the formula, so a changed sign or a
    // divide-turned-multiply has nowhere to hide. This is the number that
    // reaches Garmin Connect as the ride's training load.

    /// A ride of `secs` seconds at 1 Hz, every point identical.
    fn flat_ride(secs: u32, watts: Option<u32>, hr: Option<u32>) -> Session {
        let mut session = Session::new(None);
        session.data_points = (0..secs)
            .map(|i| DataPoint {
                elapsed_secs: i,
                power_watts: watts,
                target_watts: None,
                heart_rate_bpm: hr,
                cadence_rpm: None,
                speed_kmh: None,
                lat: None,
                lng: None,
                altitude_m: None,
            })
            .collect();
        session
    }

    #[track_caller]
    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-3 * expected.abs().max(1.0),
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn should_read_the_gap_to_the_next_sample() {
        let session = {
            let mut s = Session::new(None);
            for secs in [0u32, 5, 105] {
                s.data_points.push(DataPoint {
                    elapsed_secs: secs,
                    power_watts: None,
                    target_watts: None,
                    heart_rate_bpm: None,
                    cadence_rpm: None,
                    speed_kmh: None,
                    lat: None,
                    lng: None,
                    altitude_m: None,
                });
            }
            s
        };
        let pts = &session.data_points;
        assert_eq!(sample_secs(pts, 0), 5.0);
        // A 100 s gap is a clock jump, not a hundred seconds of riding.
        assert_eq!(sample_secs(pts, 1), 60.0);
        // Nothing follows the last point, so it stands for the 1 Hz recorded.
        assert_eq!(sample_secs(pts, 2), 1.0);
    }

    #[test]
    fn should_map_a_load_onto_the_firstbeat_scale_it_was_fitted_to() {
        // The two reference rides the curve was fitted against.
        assert_close(aerobic_te_for_load(202.0), 1.458 * 202.0f32.ln() - 3.34);
        assert_close(aerobic_te_for_load(48.0), 1.458 * 48.0f32.ln() - 3.34);
        // Under a load of 1 there is no effect to report, and no logarithm to take.
        assert_eq!(aerobic_te_for_load(1.0), 0.0);
        assert_eq!(aerobic_te_for_load(0.0), 0.0);
    }

    #[test]
    fn should_not_count_a_zero_reading_as_a_heart_rate() {
        // Half the ride reads 0 bpm — the strap saying nothing, not a heart
        // beating slowly. Counting those gives a full-coverage trace with a
        // 100 bpm spread, which would sail through both gates.
        let mut session = flat_ride(10, Some(200), Some(0));
        for p in session.data_points.iter_mut().skip(5) {
            p.heart_rate_bpm = Some(100);
        }
        assert!(!hr_is_usable(&session));
    }

    #[test]
    fn should_reject_a_trace_that_covers_too_little_of_the_ride() {
        // Four readings in ten seconds is 40 % coverage, under the half the
        // integration needs. The spread is wide enough to pass the other gate.
        let mut session = flat_ride(10, Some(200), None);
        for (i, p) in session.data_points.iter_mut().take(4).enumerate() {
            p.heart_rate_bpm = Some(120 + i as u32 * 10);
        }
        assert!(!hr_is_usable(&session));
    }

    #[test]
    fn should_accept_a_trace_that_covers_half_the_ride_with_a_real_spread() {
        let mut session = flat_ride(10, Some(200), None);
        for (i, p) in session.data_points.iter_mut().take(5).enumerate() {
            p.heart_rate_bpm = Some(120 + i as u32 * 10);
        }
        assert!(hr_is_usable(&session));
    }

    #[test]
    fn should_integrate_the_heart_rate_reserve_second_by_second() {
        // Span 100 bpm, so the two heart rates are 0.45 and 0.55 of reserve
        // exactly, and 600 seconds of them is a sum that can be written down.
        let profile = AthleteProfile {
            max_hr: 200,
            resting_hr: 100,
            ..athlete()
        };
        let mut session = flat_ride(600, None, Some(145));
        for p in session.data_points.iter_mut().skip(300) {
            p.heart_rate_bpm = Some(155);
        }
        let est = estimate(&session, &profile).expect("a usable trace");
        assert_eq!(est.source, LoadSource::HeartRate);
        let expected = 300.0 * trimp_increment(0.45, 1.0) + 300.0 * trimp_increment(0.55, 1.0);
        assert_close(est.load, expected);
        assert_close(est.aerobic_te, aerobic_te_for_load(expected));
    }

    #[test]
    fn should_map_power_onto_the_reserve_it_was_fitted_to() {
        // Constant power, so the 90 s smoothing has nothing to smooth and the
        // reserve is the fitted line read at half of FTP.
        let est =
            estimate(&flat_ride(600, Some(100), None), &athlete()).expect("a ride with power");
        assert_eq!(est.source, LoadSource::Power);
        let reserve = HR_RESERVE_AT_ZERO + HR_RESERVE_PER_FTP * 0.5;
        assert_close(est.load, 600.0 * trimp_increment(reserve, 1.0));
    }

    #[test]
    fn should_have_nothing_to_estimate_from_without_an_ftp() {
        // Power with no FTP to read it against is not an effort, it is a number.
        let profile = AthleteProfile {
            ftp_watts: 0,
            ..athlete()
        };
        let mut session = flat_ride(600, Some(200), None);
        session.ftp_watts = Some(0);
        assert!(estimate(&session, &profile).is_none());
    }

    #[test]
    fn should_score_time_above_threshold_by_how_far_above_it_went() {
        // FTP 100 and a steady 200 W: every second sits 0.95 above the 1.05
        // threshold, so the proxy is 0.95² per second over 600 seconds.
        let mut session = flat_ride(600, Some(200), None);
        session.ftp_watts = Some(100);
        let est = estimate(&session, &athlete()).expect("a ride with power");
        let excess = 2.0 - ANAEROBIC_THRESHOLD_FRAC;
        let proxy = excess * excess * 600.0;
        assert_close(est.anaerobic_te, 0.506 * proxy.ln() - 0.40);
    }

    #[test]
    fn should_score_no_anaerobic_effect_at_the_threshold_itself() {
        // Exactly 1.05 × FTP contributes nothing: the excess over threshold is
        // zero, whichever side of the comparison it falls.
        let mut session = flat_ride(600, Some(105), None);
        session.ftp_watts = Some(100);
        let est = estimate(&session, &athlete()).expect("a ride with power");
        assert_eq!(est.anaerobic_te, 0.0);
    }

    #[test]
    fn should_smooth_a_step_in_power_the_way_heart_rate_lags_it() {
        // Every test above rides at constant power, where the smoothing has
        // nothing to do and its arithmetic cannot be wrong. Power steps here.
        let mut session = flat_ride(4, Some(100), None);
        for p in session.data_points.iter_mut().skip(1) {
            p.power_watts = Some(300);
        }
        let est = estimate(&session, &athlete()).expect("a ride with power");

        // The same walk written out: the first sample is taken as it stands,
        // and each later one moves a 90th of the way to the new reading.
        let ftp = 200.0;
        let mut smoothed = 100.0f32;
        let reserve_at = |w: f32| HR_RESERVE_AT_ZERO + HR_RESERVE_PER_FTP * (w / ftp);
        let mut expected = trimp_increment(reserve_at(smoothed), 1.0);
        for _ in 1..4 {
            smoothed += (300.0 - smoothed) * (1.0 / POWER_SMOOTHING_SECS);
            expected += trimp_increment(reserve_at(smoothed), 1.0);
        }
        assert_close(est.load, expected);
    }

    #[test]
    fn should_weight_time_above_threshold_by_the_gap_between_samples() {
        // A trace recorded every two seconds spends two seconds above
        // threshold per sample, not one.
        let mut session = flat_ride(300, Some(200), None);
        for (i, p) in session.data_points.iter_mut().enumerate() {
            p.elapsed_secs = i as u32 * 2;
        }
        session.ftp_watts = Some(100);
        let est = estimate(&session, &athlete()).expect("a ride with power");
        let excess = 2.0 - ANAEROBIC_THRESHOLD_FRAC;
        // 299 gaps of two seconds, and a last sample standing for one.
        let proxy = excess * excess * (299.0 * 2.0 + 1.0);
        assert_close(est.anaerobic_te, 0.506 * proxy.ln() - 0.40);
    }
}
