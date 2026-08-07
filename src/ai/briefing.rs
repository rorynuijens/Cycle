use crate::ai::coach::{WellnessSnapshot, WorkoutOption};
use crate::data::athlete::AthleteProfile;

pub struct PlannedWorkout {
    pub name: String,
    pub duration_mins: u32,
    pub tss: f32,
    pub category: String,
}

pub struct BriefingContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
    pub today_wellness: Option<WellnessSnapshot>,
    pub planned_workout: Option<PlannedWorkout>,
    pub workout_options: Vec<WorkoutOption>,
    pub athlete_context: String,
    /// Upcoming dates marked as time off (YYYY-MM-DD strings, next 14 days).
    pub time_off_dates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BriefingDecision {
    Proceed,
    Modify,
    Rest,
}

impl std::fmt::Display for BriefingDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proceed => write!(f, "Proceed"),
            Self::Modify => write!(f, "Modify"),
            Self::Rest => write!(f, "Rest"),
        }
    }
}

/// Parse the AI decision from the briefing text.
pub fn parse_briefing_decision(text: &str) -> BriefingDecision {
    let upper = text.to_uppercase();
    if let Some(pos) = upper.find("DECISION:") {
        let after = upper[pos + 9..].trim_start().to_string();
        if after.starts_with("TIME_OFF") || after.starts_with("TIME OFF") {
            return BriefingDecision::Rest; // treat time off as rest in the UI
        } else if after.starts_with("PROCEED") {
            return BriefingDecision::Proceed;
        } else if after.starts_with("MODIFY") {
            return BriefingDecision::Modify;
        } else if after.starts_with("REST") {
            return BriefingDecision::Rest;
        }
    }
    // Fallback: scan for keywords
    if upper.contains("TIME OFF") || upper.contains("\nREST") || upper.contains("REST DAY") {
        BriefingDecision::Rest
    } else if upper.contains("MODIFY") || upper.contains("ALTERNATIVE") {
        BriefingDecision::Modify
    } else {
        BriefingDecision::Proceed
    }
}

/// Extract the alternative workout name from a Modify briefing.
pub fn parse_alternative_workout(text: &str) -> Option<String> {
    crate::ai::naming::extract_marker_value(text, "ALTERNATIVE_WORKOUT:")
}

pub fn build_briefing_prompt(ctx: &BriefingContext) -> String {
    let today_str = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let is_today_time_off = ctx.time_off_dates.contains(&today_str);

    // Same thresholds the Fitness page shows the rider — see training/fitness.rs.
    // This table used to run 15 points fresher than the page, so the briefing
    // could call a Form of +3 "good form" while the page called it fatigue.
    let tsb_desc = crate::training::fitness::TsbBand::of(ctx.tsb).prompt_description();

    let wellness_str = match &ctx.today_wellness {
        Some(w) => {
            let mut parts = Vec::new();
            if let Some(v) = w.hrv {
                parts.push(format!("HRV {v:.0}"));
            }
            if let Some(v) = w.resting_hr {
                parts.push(format!("resting HR {v} bpm"));
            }
            if let Some(h) = w.sleep_hours {
                let sc = w
                    .sleep_score
                    .map(|s| format!(" (score {s})"))
                    .unwrap_or_default();
                parts.push(format!("sleep {h:.1} h{sc}"));
            }
            if parts.is_empty() {
                "No wellness readings this morning.".to_string()
            } else {
                parts.join(", ")
            }
        }
        None => "No wellness data available.".to_string(),
    };

    let planned_str = match &ctx.planned_workout {
        Some(p) => format!(
            "{} ({}, {} min, TSS {:.0})",
            p.name, p.category, p.duration_mins, p.tss
        ),
        None => "No workout scheduled for today.".to_string(),
    };

    let options_str = if ctx.workout_options.is_empty() {
        "  No alternatives in library.".to_string()
    } else {
        ctx.workout_options
            .iter()
            .map(|w| {
                format!(
                    "  - {} ({}, {} min, TSS {:.0})",
                    w.name, w.category, w.duration_mins, w.tss
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let context_section = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    // Build time-off block — today-is-off gets a prominent banner; future dates get a list.
    let time_off_section = if ctx.time_off_dates.is_empty() {
        String::new()
    } else if is_today_time_off {
        let future_dates: Vec<&String> = ctx
            .time_off_dates
            .iter()
            .filter(|d| d.as_str() != today_str.as_str())
            .collect();
        let mut s = "\nTODAY IS A SCHEDULED TIME OFF DAY — no indoor cycling. \
                      In the Recommendation section, prescribe a non-cycling activity \
                      (e.g. walk, yoga, easy swim, stretching) instead of a workout."
            .to_string();
        if !future_dates.is_empty() {
            s.push_str(&format!(
                "\n\nAdditional time off (no cycling on these days):\n{}",
                future_dates
                    .iter()
                    .map(|d| format!("  - {d}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        s
    } else {
        format!(
            "\nUPCOMING TIME OFF (no indoor cycling on these dates):\n{}\n\
             Do NOT schedule or suggest indoor cycling workouts on any of these dates.",
            ctx.time_off_dates
                .iter()
                .map(|d| format!("  - {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    let nutrition_section = if ctx.planned_workout.is_some() {
        let dur = ctx
            .planned_workout
            .as_ref()
            .map(|p| p.duration_mins)
            .unwrap_or(0);
        let tss = ctx.planned_workout.as_ref().map(|p| p.tss).unwrap_or(0.0);
        format!(
            r#"

NUTRITION GUIDANCE REQUEST:
For this planned workout ({dur} min, TSS {tss:.0}), provide specific pre/intra/post fueling targets:
- Pre (2–3 h before): carbohydrate grams, fluids ml, timing
- Intra (if > 60 min): carbs g/hour, fluids ml/hour
- Post: carbs g, protein g, fluids ml within 30 minutes

Suggest specific Turkish foods and portions:
  Pre: pilav (rice), makarna (pasta), ekmek (bread), simit, yulaf ezmesi (oatmeal)
  Intra: banan (banana), hurma (dates), portakal suyu (orange juice), kuru üzüm (raisins)
  Post protein: yoğurt, lor peyniri, beyaz peynir, yumurta (eggs), mercimek çorbası (lentil soup)
  Hydration/electrolytes: ayran, şalgam suyu, limonata with a pinch of salt"#
        )
    } else {
        String::new()
    };

    format!(
        r#"You are an expert multisport endurance coach and sports nutritionist. The athlete's primary sport is indoor cycling, but their training also includes runs, swims, and other activities — CTL/ATL reflect all sport types. This is their daily morning briefing.

{context_section}ATHLETE: FTP {ftp} W · {weight:.1} kg · max HR {max_hr} bpm

TODAY'S TRAINING STATUS:
- CTL (fitness): {ctl:.0}
- ATL (fatigue): {atl:.0}
- TSB (form): {tsb:+.0} — {tsb_desc}
{time_off}
TODAY'S WELLNESS (from this morning):
{wellness}

TODAY'S PLANNED WORKOUT:
{planned}

ALTERNATIVE WORKOUTS FOR MODIFICATION:
{options}
{nutrition_section}

Provide a morning briefing with these sections:

1. **Readiness**: 2 sentences combining TSB and today's wellness signals into one clear picture.
2. **Recommendation**: Proceed, Modify, Rest, or Time Off — one sentence of reasoning.
   - Proceed: the planned workout is appropriate
   - Modify: swap to a lighter workout (cite the exact alternative name)
   - Rest: skip training — active recovery only
   - Time Off: today is a scheduled time off day — recommend one non-cycling activity (walk, yoga, swim, etc.) and skip all workout fueling sections
3. **Pre-workout fueling** (skip if Rest or Time Off): specific Turkish foods with gram quantities and timing.
4. **Intra-workout** (only if planned duration > 60 min and not Rest/Time Off): carbs/fluids per hour with Turkish food examples.
5. **Post-workout recovery** (skip if Time Off): protein and carb targets, specific Turkish meal suggestions.

Under 350 words. Be direct — this is read before breakfast.

End with exactly one of:
DECISION: PROCEED
DECISION: MODIFY
DECISION: REST
DECISION: TIME_OFF

If MODIFY, also add on a new line:
ALTERNATIVE_WORKOUT: <exact workout name from alternatives>"#,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        max_hr = ctx.athlete.max_hr,
        ctl = ctx.ctl,
        atl = ctx.atl,
        tsb = ctx.tsb,
        tsb_desc = tsb_desc,
        wellness = wellness_str,
        planned = planned_str,
        time_off = time_off_section,
        options = options_str,
        nutrition_section = nutrition_section,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> BriefingContext {
        BriefingContext {
            athlete: AthleteProfile::default(),
            ctl: 50.0,
            atl: 45.0,
            tsb: 5.0,
            today_wellness: None,
            planned_workout: None,
            workout_options: Vec::new(),
            athlete_context: String::new(),
            time_off_dates: Vec::new(),
        }
    }

    // ── parse_briefing_decision ──────────────────────────────────────────────────

    #[test]
    fn decision_proceed_from_explicit_marker() {
        assert_eq!(
            parse_briefing_decision("...\nDECISION: PROCEED"),
            BriefingDecision::Proceed
        );
    }

    #[test]
    fn decision_modify_from_explicit_marker() {
        assert_eq!(
            parse_briefing_decision("DECISION: MODIFY\nALTERNATIVE_WORKOUT: X"),
            BriefingDecision::Modify
        );
    }

    #[test]
    fn decision_rest_from_explicit_marker() {
        assert_eq!(
            parse_briefing_decision("DECISION: REST"),
            BriefingDecision::Rest
        );
    }

    #[test]
    fn decision_time_off_maps_to_rest() {
        assert_eq!(
            parse_briefing_decision("DECISION: TIME_OFF"),
            BriefingDecision::Rest
        );
        assert_eq!(
            parse_briefing_decision("DECISION: TIME OFF"),
            BriefingDecision::Rest
        );
    }

    #[test]
    fn decision_marker_is_case_insensitive() {
        assert_eq!(
            parse_briefing_decision("decision: proceed"),
            BriefingDecision::Proceed
        );
    }

    #[test]
    fn decision_falls_back_to_keywords_without_marker() {
        assert_eq!(
            parse_briefing_decision("You should take a rest day today."),
            BriefingDecision::Rest
        );
        assert_eq!(
            parse_briefing_decision("Consider an alternative session."),
            BriefingDecision::Modify
        );
    }

    #[test]
    fn decision_defaults_to_proceed() {
        assert_eq!(
            parse_briefing_decision("Looks like a solid day to train."),
            BriefingDecision::Proceed
        );
    }

    // ── parse_alternative_workout ────────────────────────────────────────────────

    #[test]
    fn alternative_workout_extracts_first_line_name() {
        let text = "DECISION: MODIFY\nALTERNATIVE_WORKOUT: Sweet Spot 2x20\nReasoning here";
        assert_eq!(
            parse_alternative_workout(text),
            Some("Sweet Spot 2x20".to_string())
        );
    }

    #[test]
    fn alternative_workout_none_without_marker() {
        assert!(parse_alternative_workout("DECISION: PROCEED").is_none());
    }

    #[test]
    fn alternative_workout_none_when_name_empty() {
        assert!(parse_alternative_workout("ALTERNATIVE_WORKOUT:   \n").is_none());
    }

    // ── build_briefing_prompt ────────────────────────────────────────────────────

    #[test]
    fn prompt_includes_decision_instructions_and_athlete_data() {
        let prompt = build_briefing_prompt(&ctx());
        assert!(prompt.contains("DECISION: PROCEED"));
        assert!(prompt.contains("FTP 200 W"));
    }

    #[test]
    fn prompt_omits_nutrition_when_no_planned_workout() {
        let prompt = build_briefing_prompt(&ctx());
        assert!(!prompt.contains("NUTRITION GUIDANCE REQUEST"));
    }

    #[test]
    fn prompt_includes_nutrition_when_workout_planned() {
        let mut c = ctx();
        c.planned_workout = Some(PlannedWorkout {
            name: "Threshold 3x12".into(),
            duration_mins: 60,
            tss: 75.0,
            category: "Threshold".into(),
        });
        let prompt = build_briefing_prompt(&c);
        assert!(prompt.contains("NUTRITION GUIDANCE REQUEST"));
    }

    #[test]
    fn prompt_flags_today_as_time_off_day() {
        let mut c = ctx();
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        c.time_off_dates = vec![today];
        let prompt = build_briefing_prompt(&c);
        assert!(prompt.contains("TODAY IS A SCHEDULED TIME OFF DAY"));
    }
}
