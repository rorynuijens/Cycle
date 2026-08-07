//! Whether a given workout suits the rider today.
//!
//! The library shows a recommendation badge and a line of reasoning against
//! every workout. The rules behind that used to live in the page drawing the
//! badges, untested; they are plain data in, plain data out here — no GTK
//! (CLAUDE.md §2.6).
//!
//! The order matters and is deliberate: fatigue overrides everything, then the
//! rider's stated goals, and only with no goals set does freshness decide.

use crate::data::workout::{Workout, WorkoutCategory};
use crate::training::fitness::TsbBand;

/// CTL below this counts as "no training history worth reasoning from".
const MIN_CTL_FOR_ADVICE: f64 = 1.0;

/// How a workout sits against the rider's current form and goals.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutFit {
    /// Whether to badge this workout as a good choice today.
    pub recommended: bool,
    /// Short form summary, e.g. "Fresh (form +12)". Empty with no history.
    pub form_text: String,
    /// One sentence explaining the verdict, shown under the badge.
    pub rationale: String,
}

/// Keyword sets matched against the rider's goals, lower-cased.
const BASE_WORDS: [&str; 5] = ["base", "aerobic", "zone 2", "endurance", "foundation"];
const FTP_WORDS: [&str; 4] = ["ftp", "threshold", "time trial", "tt"];
const RACE_WORDS: [&str; 4] = ["race", "event", "competition", "racing"];
const POWER_WORDS: [&str; 5] = ["power", "sprint", "vo2", "climbing", "intervals"];

/// Judge `workout` against the rider's fitness (`ctl`), form (`tsb`) and goals.
///
/// `goals` are matched as lower-cased substrings, so they must already be
/// lower-cased by the caller.
pub fn workout_fit(workout: &Workout, ctl: f64, tsb: f64, goals: &[String]) -> WorkoutFit {
    if ctl < MIN_CTL_FOR_ADVICE {
        return WorkoutFit {
            recommended: false,
            form_text: "No training data yet".into(),
            rationale: "Record a few sessions to unlock personalised recommendations.".into(),
        };
    }

    // Same bands as the Fitness page and the coach — see training/fitness.rs.
    let band = TsbBand::of(tsb);
    let form_text = format!("{} (form {:+.0})", band.short_label(), tsb);

    // Fatigue overrides goal matching — recovery comes first.
    if band.is_fatigued() {
        let easy = matches!(
            workout.category,
            WorkoutCategory::Recovery | WorkoutCategory::Endurance
        );
        return WorkoutFit {
            recommended: easy,
            form_text,
            rationale: if easy {
                "Easy aerobic work is the most productive choice when you're this fatigued.".into()
            } else {
                "You're carrying significant fatigue — a recovery or easy endurance session \
                 would serve you better right now."
                    .into()
            },
        };
    }

    let goals_text = goals.join(" ");
    let wants = |keywords: &[&str]| keywords.iter().any(|kw| goals_text.contains(kw));

    if wants(&BASE_WORDS) {
        let fits = matches!(
            workout.category,
            WorkoutCategory::Recovery | WorkoutCategory::Endurance | WorkoutCategory::Tempo
        );
        return WorkoutFit {
            recommended: fits,
            form_text,
            rationale: if fits {
                "Aligns with your aerobic base goal — Z1–Z3 work builds the engine.".into()
            } else {
                format!(
                    "Your goal targets aerobic base; {} work is above the ideal intensity \
                     range for base building.",
                    workout.category.label()
                )
            },
        };
    }

    let wants_race = wants(&RACE_WORDS);
    if wants(&FTP_WORDS) || wants_race {
        let fits = matches!(
            workout.category,
            WorkoutCategory::SweetSpot | WorkoutCategory::Threshold | WorkoutCategory::Vo2Max
        );
        return WorkoutFit {
            recommended: fits,
            form_text,
            rationale: if fits {
                if wants_race {
                    "Solid race-preparation work at the right intensity.".into()
                } else {
                    "Directly targets the FTP gains in your goal.".into()
                }
            } else {
                "Your goal calls for threshold-range work, but any training contributes.".into()
            },
        };
    }

    if wants(&POWER_WORDS) {
        let fits = matches!(
            workout.category,
            WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic | WorkoutCategory::Threshold
        );
        return WorkoutFit {
            recommended: fits,
            form_text,
            rationale: if fits {
                "Targets the power output in your goal.".into()
            } else {
                "Your power goal favours high-intensity work, though a broad base helps too.".into()
            },
        };
    }

    // No goals set — fall back to freshness.
    if band.is_fresh() {
        let hard = matches!(
            workout.category,
            WorkoutCategory::Threshold | WorkoutCategory::Vo2Max | WorkoutCategory::Anaerobic
        );
        return WorkoutFit {
            recommended: hard,
            form_text,
            rationale: if hard {
                "You're well rested — a great time for a quality hard session.".into()
            } else {
                "You're fresh. Any training works; your form suits hard efforts particularly well."
                    .into()
            },
        };
    }

    WorkoutFit {
        recommended: false,
        form_text,
        rationale: "Add a training goal in the Coaching tab to get targeted recommendations."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;

    fn workout(category: WorkoutCategory) -> Workout {
        Workout::from_segments(
            "Test",
            "",
            category,
            vec![Segment::steady(3600, 90.0, "Main")],
        )
    }

    fn goals(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    /// TSB values that land in each band — see `TsbBand`.
    const FATIGUED: f64 = -30.0;
    const NEUTRAL: f64 = 0.0;
    const FRESH: f64 = 20.0;

    // ── no history ───────────────────────────────────────────────────────────

    #[test]
    fn should_decline_to_advise_without_training_history() {
        let fit = workout_fit(&workout(WorkoutCategory::Threshold), 0.0, 0.0, &[]);
        assert!(!fit.recommended);
        assert_eq!(fit.form_text, "No training data yet");
        assert!(fit.rationale.contains("Record a few sessions"));
    }

    #[test]
    fn should_start_advising_once_there_is_any_fitness() {
        let fit = workout_fit(&workout(WorkoutCategory::Endurance), 1.0, NEUTRAL, &[]);
        assert_ne!(fit.form_text, "No training data yet");
    }

    // ── fatigue wins ─────────────────────────────────────────────────────────

    #[test]
    fn should_recommend_easy_work_when_fatigued() {
        for category in [WorkoutCategory::Recovery, WorkoutCategory::Endurance] {
            let fit = workout_fit(&workout(category), 50.0, FATIGUED, &[]);
            assert!(fit.recommended, "{category:?} should be recommended");
        }
    }

    #[test]
    fn should_refuse_hard_work_when_fatigued() {
        for category in [WorkoutCategory::Vo2Max, WorkoutCategory::Threshold] {
            let fit = workout_fit(&workout(category), 50.0, FATIGUED, &[]);
            assert!(!fit.recommended, "{category:?} should not be recommended");
            assert!(fit.rationale.contains("fatigue"));
        }
    }

    #[test]
    fn should_let_fatigue_override_a_matching_goal() {
        // The rider's goal says threshold work; their body says otherwise.
        let fit = workout_fit(
            &workout(WorkoutCategory::Threshold),
            50.0,
            FATIGUED,
            &goals(&["raise my ftp"]),
        );
        assert!(!fit.recommended, "recovery must outrank the goal");
    }

    // ── goal matching ────────────────────────────────────────────────────────

    #[test]
    fn should_match_an_aerobic_base_goal_to_easy_categories() {
        let g = goals(&["build aerobic base"]);
        for category in [
            WorkoutCategory::Recovery,
            WorkoutCategory::Endurance,
            WorkoutCategory::Tempo,
        ] {
            assert!(workout_fit(&workout(category), 50.0, NEUTRAL, &g).recommended);
        }
        assert!(!workout_fit(&workout(WorkoutCategory::Vo2Max), 50.0, NEUTRAL, &g).recommended);
    }

    #[test]
    fn should_name_the_category_when_it_overshoots_a_base_goal() {
        let fit = workout_fit(
            &workout(WorkoutCategory::Vo2Max),
            50.0,
            NEUTRAL,
            &goals(&["endurance"]),
        );
        assert!(
            fit.rationale.contains(WorkoutCategory::Vo2Max.label()),
            "{}",
            fit.rationale
        );
    }

    #[test]
    fn should_match_an_ftp_goal_to_threshold_range_work() {
        let g = goals(&["increase ftp"]);
        for category in [
            WorkoutCategory::SweetSpot,
            WorkoutCategory::Threshold,
            WorkoutCategory::Vo2Max,
        ] {
            assert!(workout_fit(&workout(category), 50.0, NEUTRAL, &g).recommended);
        }
        assert!(!workout_fit(&workout(WorkoutCategory::Recovery), 50.0, NEUTRAL, &g).recommended);
    }

    #[test]
    fn should_word_a_race_goal_differently_from_an_ftp_goal() {
        let race = workout_fit(
            &workout(WorkoutCategory::Threshold),
            50.0,
            NEUTRAL,
            &goals(&["target race in june"]),
        );
        let ftp = workout_fit(
            &workout(WorkoutCategory::Threshold),
            50.0,
            NEUTRAL,
            &goals(&["raise ftp"]),
        );
        assert!(race.recommended && ftp.recommended);
        assert!(race.rationale.contains("race-preparation"));
        assert!(ftp.rationale.contains("FTP gains"));
    }

    #[test]
    fn should_match_a_power_goal_to_high_intensity_work() {
        let g = goals(&["improve sprint power"]);
        for category in [
            WorkoutCategory::Vo2Max,
            WorkoutCategory::Anaerobic,
            WorkoutCategory::Threshold,
        ] {
            assert!(workout_fit(&workout(category), 50.0, NEUTRAL, &g).recommended);
        }
        assert!(!workout_fit(&workout(WorkoutCategory::Recovery), 50.0, NEUTRAL, &g).recommended);
    }

    #[test]
    fn should_check_base_goals_before_ftp_goals() {
        // "endurance" and "threshold" both appear; base is checked first, so a
        // Tempo session is recommended and a VO2 session is not.
        let g = goals(&["endurance base", "threshold work"]);
        assert!(workout_fit(&workout(WorkoutCategory::Tempo), 50.0, NEUTRAL, &g).recommended);
        assert!(!workout_fit(&workout(WorkoutCategory::Vo2Max), 50.0, NEUTRAL, &g).recommended);
    }

    #[test]
    fn should_match_a_goal_anywhere_in_the_sentence() {
        let fit = workout_fit(
            &workout(WorkoutCategory::Endurance),
            50.0,
            NEUTRAL,
            &goals(&["i would like to build a solid aerobic engine this winter"]),
        );
        assert!(fit.recommended);
    }

    #[test]
    fn should_match_across_separate_goals() {
        // Goals are joined before matching, so any one of them can hit.
        let g = goals(&["lose weight", "raise ftp"]);
        assert!(workout_fit(&workout(WorkoutCategory::Threshold), 50.0, NEUTRAL, &g).recommended);
    }

    // ── no goals: freshness decides ──────────────────────────────────────────

    #[test]
    fn should_recommend_hard_work_when_fresh_and_goalless() {
        for category in [
            WorkoutCategory::Threshold,
            WorkoutCategory::Vo2Max,
            WorkoutCategory::Anaerobic,
        ] {
            assert!(workout_fit(&workout(category), 50.0, FRESH, &[]).recommended);
        }
    }

    #[test]
    fn should_recommend_nothing_in_particular_when_neutral_and_goalless() {
        let fit = workout_fit(&workout(WorkoutCategory::Threshold), 50.0, NEUTRAL, &[]);
        assert!(!fit.recommended);
        assert!(fit.rationale.contains("Add a training goal"));
    }

    #[test]
    fn should_report_form_alongside_every_verdict() {
        let fit = workout_fit(&workout(WorkoutCategory::Tempo), 50.0, FRESH, &[]);
        assert!(fit.form_text.contains("form +20"), "{}", fit.form_text);
    }
}
