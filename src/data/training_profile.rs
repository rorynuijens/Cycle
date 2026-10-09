//! What the rider told the program builder about how they train.
//!
//! Every answer is a choice from a fixed list, never free text, so each one maps
//! to a fixed instruction for the coach. That keeps the prompt predictable and
//! testable: the same answers always produce the same words.
//!
//! "Not sure" and "let the coach choose" add nothing to the prompt. A rider who
//! does not know is planned for exactly as before the profile existed.

use anyhow::Result;
use chrono::{NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

/// The earliest week an event may fall in. An event in week 1 or 2 leaves no
/// time to build anything before the taper begins.
pub const MIN_EVENT_WEEK: u32 = 3;

/// The latest week an event may fall in. Further out, a plan written today is
/// guesswork by the time the rider reaches the end of it.
pub const MAX_EVENT_WEEK: u32 = 52;

/// Weeks planned when the rider wants no end date — the program rolls over
/// when it runs out.
pub const ROLLING_WEEKS: u32 = 8;

/// What the rider is training for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Goal {
    ClimbingEvent,
    LongEvent,
    Racing,
    TimeTrial,
    GetFaster,
    Fitness,
}

impl Goal {
    pub const ALL: [Goal; 6] = [
        Goal::ClimbingEvent,
        Goal::LongEvent,
        Goal::Racing,
        Goal::TimeTrial,
        Goal::GetFaster,
        Goal::Fitness,
    ];

    /// Whether this goal is a dated event the program builds towards.
    pub fn has_event(self) -> bool {
        matches!(
            self,
            Goal::ClimbingEvent | Goal::LongEvent | Goal::Racing | Goal::TimeTrial
        )
    }

    fn prompt_line(self) -> &'static str {
        match self {
            Goal::ClimbingEvent => {
                "Goal: a climbing event or hilly gran fondo. Prioritise sustained \
                 threshold and sweet-spot-length climbing efforts (10–40 min) and \
                 long endurance rides."
            }
            Goal::LongEvent => {
                "Goal: a long flat or rolling event. Prioritise endurance volume and \
                 tempo durability; keep some threshold work."
            }
            Goal::Racing => {
                "Goal: racing. Include VO₂max and short anaerobic efforts with repeated \
                 surges on a solid endurance base."
            }
            Goal::TimeTrial => {
                "Goal: a time trial or triathlon bike leg. Prioritise sustained \
                 threshold and steady sub-threshold power; few short sprints."
            }
            Goal::GetFaster => {
                "Goal: no event — raise FTP. Progress threshold and VO₂max work \
                 steadily on an endurance base."
            }
            Goal::Fitness => {
                "Goal: no event — general fitness and health. Keep most sessions \
                 endurance and tempo, with moderate, enjoyable intensity."
            }
        }
    }
}

/// How the rider wants intensity distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approach {
    CoachChooses,
    Polarised,
    Pyramidal,
    SweetSpot,
}

impl Approach {
    pub const ALL: [Approach; 4] = [
        Approach::CoachChooses,
        Approach::Polarised,
        Approach::Pyramidal,
        Approach::SweetSpot,
    ];

    fn prompt_line(self) -> Option<&'static str> {
        match self {
            Approach::CoachChooses => None,
            Approach::Polarised => Some(
                "Approach: polarised. About 80 % of sessions recovery or endurance, the \
                 rest VO₂max or threshold. No sweet spot or tempo sessions.",
            ),
            Approach::Pyramidal => Some(
                "Approach: pyramidal. Mostly endurance, a smaller share of tempo and \
                 sweet spot, and the least at threshold and above.",
            ),
            Approach::SweetSpot => Some(
                "Approach: sweet spot and threshold. The rider is time-crunched: make \
                 most build sessions sweet spot or threshold, with short endurance rides \
                 between them.",
            ),
        }
    }
}

/// How the rider feels the day after a hard session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    NextDay,
    OneEasyDay,
    TwoPlusEasyDays,
    NotSure,
}

impl Recovery {
    pub const ALL: [Recovery; 4] = [
        Recovery::NextDay,
        Recovery::OneEasyDay,
        Recovery::TwoPlusEasyDays,
        Recovery::NotSure,
    ];

    fn prompt_line(self) -> Option<&'static str> {
        match self {
            Recovery::NextDay => {
                Some("Recovery: the rider feels fresh the day after a hard session.")
            }
            Recovery::OneEasyDay => {
                Some("Recovery: the rider needs one easy or rest day after each hard session.")
            }
            Recovery::TwoPlusEasyDays => Some(
                "Recovery: the rider needs at least two easy or rest days after each hard \
                 session.",
            ),
            Recovery::NotSure => None,
        }
    }
}

/// Whether two hard days in a row suit the rider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackToBack {
    Fine,
    Avoid,
    NotSure,
}

impl BackToBack {
    pub const ALL: [BackToBack; 3] = [BackToBack::Fine, BackToBack::Avoid, BackToBack::NotSure];

    fn prompt_line(self) -> Option<&'static str> {
        match self {
            BackToBack::Fine => Some("Back-to-back hard days are fine for this rider."),
            BackToBack::Avoid => Some("Never schedule two hard sessions on consecutive days."),
            BackToBack::NotSure => None,
        }
    }
}

/// How long the rider has trained with structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Experience {
    New,
    OneToThree,
    ThreePlus,
}

impl Experience {
    pub const ALL: [Experience; 3] = [
        Experience::New,
        Experience::OneToThree,
        Experience::ThreePlus,
    ];

    fn prompt_line(self) -> &'static str {
        match self {
            Experience::New => {
                "Experience: under a year of structured training. Progress conservatively \
                 and keep interval sessions simple."
            }
            Experience::OneToThree => "Experience: one to three years of structured training.",
            Experience::ThreePlus => "Experience: over three years of structured training.",
        }
    }
}

/// How long a program with no event runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Length {
    FourWeeks,
    EightWeeks,
    TwelveWeeks,
    Rolling,
}

impl Length {
    pub const ALL: [Length; 4] = [
        Length::FourWeeks,
        Length::EightWeeks,
        Length::TwelveWeeks,
        Length::Rolling,
    ];

    /// Weeks to plan, or `None` for a rolling program.
    pub fn weeks(self) -> Option<u32> {
        match self {
            Length::FourWeeks => Some(4),
            Length::EightWeeks => Some(8),
            Length::TwelveWeeks => Some(12),
            Length::Rolling => None,
        }
    }
}

/// The build:recovery cycle the program follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPattern {
    /// Three build weeks, then a recovery week — the pattern every program used
    /// before the profile existed.
    ThreeOne,
    /// Two build weeks, then a recovery week, for riders who are new or recover
    /// slowly.
    TwoOne,
}

impl BlockPattern {
    /// The sentence the program builder prompt uses.
    pub fn build_sentence(self) -> &'static str {
        match self {
            BlockPattern::ThreeOne => {
                "weeks 1–3 build load, week 4 is a recovery week (lighter workouts), then \
                 repeat"
            }
            BlockPattern::TwoOne => {
                "weeks 1–2 build load, week 3 is a recovery week (lighter workouts), then \
                 repeat"
            }
        }
    }

    /// How often the replan prompt asks for a recovery week.
    pub fn revision_cadence(self) -> &'static str {
        match self {
            BlockPattern::ThreeOne => "every fourth week",
            BlockPattern::TwoOne => "every third week",
        }
    }
}

/// Everything the program builder asks the rider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingProfile {
    pub goal: Goal,
    /// Only meaningful when [`Goal::has_event`]; ignored otherwise.
    pub event_date: Option<NaiveDate>,
    /// Only meaningful when the goal has no event.
    pub length: Length,
    pub approach: Approach,
    pub recovery: Recovery,
    pub back_to_back: BackToBack,
    pub experience: Experience,
    pub training_days: Vec<Weekday>,
}

impl Default for TrainingProfile {
    /// The answers the wizard pre-selects for a rider who has never answered.
    fn default() -> Self {
        Self {
            goal: Goal::GetFaster,
            event_date: None,
            length: Length::EightWeeks,
            approach: Approach::CoachChooses,
            recovery: Recovery::NotSure,
            back_to_back: BackToBack::NotSure,
            experience: Experience::OneToThree,
            training_days: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
        }
    }
}

impl TrainingProfile {
    /// Reads a stored profile, or `None` when the value cannot be trusted.
    ///
    /// The settings table could be damaged or edited by hand (CLAUDE.md §5.2),
    /// so a value that parses but makes no sense — no training days, or an
    /// event goal without a date — is rejected like one that does not parse.
    pub fn from_json(raw: &str) -> Option<Self> {
        let mut profile: Self = match serde_json::from_str(raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Stored training profile could not be read: {e}");
                return None;
            }
        };
        profile
            .training_days
            .sort_by_key(|d| d.num_days_from_monday());
        profile.training_days.dedup();
        if profile.training_days.is_empty() {
            tracing::warn!("Stored training profile has no training days");
            return None;
        }
        if profile.goal.has_event() && profile.event_date.is_none() {
            tracing::warn!("Stored training profile has an event goal but no date");
            return None;
        }
        Some(profile)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// The week the event falls in, counting `start_monday`'s week as week 1.
    ///
    /// Errors, with a message for the rider, when the goal has an event but no
    /// date, the event has passed, or it falls outside weeks
    /// [`MIN_EVENT_WEEK`]–[`MAX_EVENT_WEEK`].
    pub fn event_week(&self, start_monday: NaiveDate) -> Result<u32> {
        anyhow::ensure!(self.event_date.is_some(), "Choose the date of your event");
        let week = self
            .week_holding_event(start_monday)
            .ok_or_else(|| anyhow::anyhow!("That date has already passed"))?;
        anyhow::ensure!(
            week >= MIN_EVENT_WEEK,
            "That is too soon to build towards — choose a date at least two full weeks \
             after this one"
        );
        anyhow::ensure!(
            week <= MAX_EVENT_WEEK,
            "That is more than a year away — choose a date within {MAX_EVENT_WEEK} weeks"
        );
        Ok(week)
    }

    /// The week the event falls in with no bounds applied, or `None` when there
    /// is no event or it is before `start_monday`.
    ///
    /// For prompts, not for the wizard: a replan two weeks before the event must
    /// still be told about it, or the taper is lost on exactly the replan that
    /// needs it most.
    pub fn week_holding_event(&self, start_monday: NaiveDate) -> Option<u32> {
        if !self.goal.has_event() {
            return None;
        }
        let days = (self.event_date? - start_monday).num_days();
        (days >= 0).then_some((days / 7 + 1) as u32)
    }

    /// Weeks to plan from `start_monday`, or `None` for a rolling program.
    ///
    /// An event program runs up to and including the event's own week.
    pub fn weeks(&self, start_monday: NaiveDate) -> Result<Option<u32>> {
        if self.goal.has_event() {
            Ok(Some(self.event_week(start_monday)?))
        } else {
            Ok(self.length.weeks())
        }
    }

    /// The build:recovery cycle — a local rule, never asked.
    pub fn block_pattern(&self) -> BlockPattern {
        if self.experience == Experience::New || self.recovery == Recovery::TwoPlusEasyDays {
            BlockPattern::TwoOne
        } else {
            BlockPattern::ThreeOne
        }
    }

    /// The `TRAINING PROFILE` block for a prompt whose week 1 is `start_monday`.
    pub fn prompt_section(&self, start_monday: NaiveDate) -> String {
        let mut lines = vec![self.goal.prompt_line().to_string()];
        if let (Some(date), Some(week)) = (self.event_date, self.week_holding_event(start_monday)) {
            let taper = if week == 1 {
                "Taper in week 1".to_string()
            } else {
                format!("Taper in weeks {} and {week}", week - 1)
            };
            lines.push(format!(
                "The event is on {} ({}), in week {week}. {taper}: cut volume by about \
                 40 %, keep short openers, and nothing hard in the 2 days before the event. \
                 Plan nothing after week {week}.",
                date.format("%A %-d %B %Y"),
                date.format("%Y-%m-%d"),
            ));
        }
        lines.extend(self.approach.prompt_line().map(String::from));
        lines.extend(self.recovery.prompt_line().map(String::from));
        lines.extend(self.back_to_back.prompt_line().map(String::from));
        lines.push(self.experience.prompt_line().to_string());

        let body = lines
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("TRAINING PROFILE — the rider chose these; follow them:\n{body}\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    /// Monday 12 October 2026.
    fn start() -> NaiveDate {
        date(2026, 10, 12)
    }

    fn with_event(days_after_start: i64) -> TrainingProfile {
        TrainingProfile {
            goal: Goal::ClimbingEvent,
            event_date: Some(start() + Duration::days(days_after_start)),
            ..TrainingProfile::default()
        }
    }

    // ── event_week ───────────────────────────────────────────────────────────

    #[test]
    fn should_accept_an_event_on_the_monday_of_week_three() {
        assert_eq!(with_event(14).event_week(start()).unwrap(), 3);
    }

    #[test]
    fn should_reject_an_event_on_the_sunday_of_week_two() {
        assert!(with_event(13).event_week(start()).is_err());
    }

    #[test]
    fn should_reject_an_event_today() {
        assert!(with_event(0).event_week(start()).is_err());
    }

    #[test]
    fn should_reject_an_event_that_has_passed_with_its_own_message() {
        let err = with_event(-1).event_week(start()).unwrap_err().to_string();
        assert!(err.contains("already passed"), "{err}");
    }

    #[test]
    fn should_accept_an_event_on_the_last_day_of_week_52() {
        assert_eq!(with_event(52 * 7 - 1).event_week(start()).unwrap(), 52);
    }

    #[test]
    fn should_reject_an_event_on_the_first_day_of_week_53() {
        assert!(with_event(52 * 7).event_week(start()).is_err());
    }

    #[test]
    fn should_plan_through_the_event_week() {
        assert_eq!(with_event(30).weeks(start()).unwrap(), Some(5));
    }

    #[test]
    fn should_ignore_a_stale_event_date_once_the_goal_has_no_event() {
        let profile = TrainingProfile {
            goal: Goal::GetFaster,
            event_date: Some(start() - Duration::days(100)),
            length: Length::TwelveWeeks,
            ..TrainingProfile::default()
        };
        assert_eq!(profile.weeks(start()).unwrap(), Some(12));
    }

    #[test]
    fn should_plan_no_fixed_length_for_a_rolling_program() {
        let profile = TrainingProfile {
            length: Length::Rolling,
            ..TrainingProfile::default()
        };
        assert_eq!(profile.weeks(start()).unwrap(), None);
    }

    // ── block_pattern ────────────────────────────────────────────────────────

    #[test]
    fn should_pick_the_block_pattern_for_every_experience_and_recovery() {
        use BlockPattern::*;
        let expected = [
            (Experience::New, Recovery::NextDay, TwoOne),
            (Experience::New, Recovery::OneEasyDay, TwoOne),
            (Experience::New, Recovery::TwoPlusEasyDays, TwoOne),
            (Experience::New, Recovery::NotSure, TwoOne),
            (Experience::OneToThree, Recovery::NextDay, ThreeOne),
            (Experience::OneToThree, Recovery::OneEasyDay, ThreeOne),
            (Experience::OneToThree, Recovery::TwoPlusEasyDays, TwoOne),
            (Experience::OneToThree, Recovery::NotSure, ThreeOne),
            (Experience::ThreePlus, Recovery::NextDay, ThreeOne),
            (Experience::ThreePlus, Recovery::OneEasyDay, ThreeOne),
            (Experience::ThreePlus, Recovery::TwoPlusEasyDays, TwoOne),
            (Experience::ThreePlus, Recovery::NotSure, ThreeOne),
        ];
        assert_eq!(expected.len(), Experience::ALL.len() * Recovery::ALL.len());
        for (experience, recovery, pattern) in expected {
            let profile = TrainingProfile {
                experience,
                recovery,
                ..TrainingProfile::default()
            };
            assert_eq!(
                profile.block_pattern(),
                pattern,
                "{experience:?} {recovery:?}"
            );
        }
    }

    // ── prompt_section ───────────────────────────────────────────────────────

    #[test]
    fn should_add_no_line_for_answers_the_rider_was_unsure_of() {
        let section = TrainingProfile::default().prompt_section(start());
        assert_eq!(
            section,
            "TRAINING PROFILE — the rider chose these; follow them:\n\
             - Goal: no event — raise FTP. Progress threshold and VO₂max work steadily on \
             an endurance base.\n\
             - Experience: one to three years of structured training.\n\n"
        );
    }

    #[test]
    fn should_forbid_sweet_spot_when_the_rider_trains_polarised() {
        let profile = TrainingProfile {
            approach: Approach::Polarised,
            ..TrainingProfile::default()
        };
        assert!(profile.prompt_section(start()).contains(
            "- Approach: polarised. About 80 % of sessions recovery or endurance, \
                       the rest VO₂max or threshold. No sweet spot or tempo sessions.\n"
        ));
    }

    #[test]
    fn should_give_every_chosen_approach_its_own_line() {
        for approach in Approach::ALL {
            let section = TrainingProfile {
                approach,
                ..TrainingProfile::default()
            }
            .prompt_section(start());
            assert_eq!(
                section.contains("- Approach:"),
                approach != Approach::CoachChooses,
                "{approach:?}"
            );
        }
    }

    #[test]
    fn should_forbid_consecutive_hard_days_when_the_rider_avoids_them() {
        let profile = TrainingProfile {
            back_to_back: BackToBack::Avoid,
            ..TrainingProfile::default()
        };
        assert!(profile
            .prompt_section(start())
            .contains("- Never schedule two hard sessions on consecutive days.\n"));
    }

    #[test]
    fn should_name_the_event_week_and_taper_weeks() {
        // Saturday 14 November 2026 is 33 days after Monday 12 October: week 5.
        let profile = TrainingProfile {
            event_date: Some(date(2026, 11, 14)),
            ..with_event(0)
        };
        assert!(profile.prompt_section(start()).contains(
            "- The event is on Saturday 14 November 2026 (2026-11-14), in week 5. Taper in \
             weeks 4 and 5:"
        ));
    }

    #[test]
    fn should_renumber_the_event_week_from_a_later_start() {
        // A replan starts later, so the same event sits in an earlier week of it.
        let profile = TrainingProfile {
            event_date: Some(date(2026, 11, 14)),
            ..with_event(0)
        };
        let section = profile.prompt_section(start() + Duration::days(14));
        assert!(
            section.contains("in week 3. Taper in weeks 2 and 3:"),
            "{section}"
        );
    }

    #[test]
    fn should_still_name_an_event_in_week_one_when_replanning() {
        // Too close to build for, but a replan this near the event is the one
        // that most needs the taper.
        let section = with_event(3).prompt_section(start());
        assert!(section.contains("in week 1. Taper in week 1:"), "{section}");
        assert!(!section.contains("weeks 0"), "{section}");
    }

    #[test]
    fn should_leave_out_an_event_that_has_passed() {
        let section = with_event(-1).prompt_section(start());
        assert!(!section.contains("The event is on"), "{section}");
    }

    #[test]
    fn should_hold_no_event_week_for_a_goal_without_an_event() {
        let profile = TrainingProfile {
            goal: Goal::Fitness,
            ..with_event(30)
        };
        assert_eq!(profile.week_holding_event(start()), None);
    }

    // ── storage ──────────────────────────────────────────────────────────────

    #[test]
    fn should_survive_a_json_round_trip() {
        let profile = TrainingProfile {
            approach: Approach::SweetSpot,
            training_days: vec![Weekday::Tue, Weekday::Sat],
            ..with_event(40)
        };
        let json = profile.to_json().unwrap();
        assert_eq!(TrainingProfile::from_json(&json), Some(profile));
    }

    #[test]
    fn should_reject_truncated_json() {
        let json = TrainingProfile::default().to_json().unwrap();
        assert_eq!(TrainingProfile::from_json(&json[..json.len() - 2]), None);
    }

    #[test]
    fn should_reject_an_unknown_answer() {
        let json = TrainingProfile::default()
            .to_json()
            .unwrap()
            .replace("\"get_faster\"", "\"win_the_tour\"");
        assert_eq!(TrainingProfile::from_json(&json), None);
    }

    #[test]
    fn should_reject_a_profile_with_no_training_days() {
        let profile = TrainingProfile {
            training_days: Vec::new(),
            ..TrainingProfile::default()
        };
        assert_eq!(
            TrainingProfile::from_json(&profile.to_json().unwrap()),
            None
        );
    }

    #[test]
    fn should_reject_an_event_goal_without_a_date() {
        let profile = TrainingProfile {
            goal: Goal::Racing,
            event_date: None,
            ..TrainingProfile::default()
        };
        assert_eq!(
            TrainingProfile::from_json(&profile.to_json().unwrap()),
            None
        );
    }

    #[test]
    fn should_sort_and_dedupe_stored_training_days() {
        let profile = TrainingProfile {
            training_days: vec![Weekday::Fri, Weekday::Mon, Weekday::Fri],
            ..TrainingProfile::default()
        };
        let loaded = TrainingProfile::from_json(&profile.to_json().unwrap()).unwrap();
        assert_eq!(loaded.training_days, vec![Weekday::Mon, Weekday::Fri]);
    }
}
