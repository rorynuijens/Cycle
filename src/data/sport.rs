//! Sport-type labels from the services activities are imported from.
//!
//! Intervals.icu, Garmin and Strava each spell the same activity differently
//! ("Ride", "VirtualRide", "IndoorCycling"), and the raw string is stored on
//! [`crate::data::db::IntervalsActivity`] exactly as it arrived. Anything that
//! shows a sport to the rider — or tells the AI coach what they did — goes
//! through here so there is one answer rather than one per page.

/// Map a raw `sport_type` from Intervals.icu / Garmin / Strava to a clean label.
///
/// Matching is case-insensitive: the same activity comes back as `Ride` from
/// one service and `ride` from another. An empty string means cycling — the
/// app's own rides are stored without a sport. Anything unrecognised is passed
/// through unchanged, so a new sport shows its own name rather than being
/// silently relabelled as cycling.
pub fn normalize_sport_type(raw: &str) -> String {
    match raw.to_lowercase().as_str() {
        "" | "ride" | "virtualride" | "cycling" | "indoorcycling" | "mountainbiking"
        | "mountainbikeride" | "gravelride" | "ebikeride" | "handcycle" | "velomobile" => "Cycling",
        "run" | "virtualrun" | "trailrun" | "treadmillrun" => "Run",
        "walk" | "walking" => "Walk",
        "hike" | "hiking" => "Hike",
        "swim" | "swimming" | "openwaterswim" => "Swim",
        "weighttraining" | "strength" | "strengthtraining" => "Strength Training",
        "yoga" => "Yoga",
        "rowing" | "indoorrowing" => "Rowing",
        "elliptical" => "Elliptical",
        "nordicski" | "backcountryski" => "Ski",
        "workout" | "crossfit" | "hiit" => "Cross Training",
        _ => return raw.to_string(),
    }
    .to_string()
}

/// True when `raw` names a cycling activity of any kind.
pub fn is_cycling(raw: &str) -> bool {
    normalize_sport_type(raw) == "Cycling"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_treat_an_empty_sport_as_cycling() {
        // The app's own rides are stored with no sport type.
        assert_eq!(normalize_sport_type(""), "Cycling");
    }

    #[test]
    fn should_fold_every_spelling_of_cycling_together() {
        for raw in [
            "Ride",
            "VirtualRide",
            "Cycling",
            "IndoorCycling",
            "MountainBiking",
            "MountainBikeRide",
            "GravelRide",
            "EBikeRide",
        ] {
            assert_eq!(normalize_sport_type(raw), "Cycling", "for {raw}");
            assert!(is_cycling(raw), "for {raw}");
        }
    }

    #[test]
    fn should_match_regardless_of_case() {
        // Services disagree on capitalisation for the same activity.
        assert_eq!(normalize_sport_type("VIRTUALRIDE"), "Cycling");
        assert_eq!(normalize_sport_type("virtualride"), "Cycling");
        assert_eq!(normalize_sport_type("run"), "Run");
    }

    #[test]
    fn should_pass_an_unknown_sport_through_unchanged() {
        // Better to show "Padel" than to quietly call it a bike ride.
        assert_eq!(normalize_sport_type("Padel"), "Padel");
        assert!(!is_cycling("Padel"));
    }

    #[test]
    fn should_preserve_the_original_casing_of_an_unknown_sport() {
        assert_eq!(normalize_sport_type("KiteSurfing"), "KiteSurfing");
    }

    #[test]
    fn should_not_treat_running_as_cycling() {
        assert!(!is_cycling("Run"));
        assert!(!is_cycling("TrailRun"));
    }
}
