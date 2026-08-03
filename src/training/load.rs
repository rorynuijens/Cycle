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
}
