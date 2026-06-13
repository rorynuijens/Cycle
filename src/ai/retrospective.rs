use crate::ai::coach::WellnessSnapshot;
use crate::data::athlete::AthleteProfile;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RetroPeriod {
    Weekly,
    Monthly,
}

impl RetroPeriod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Weekly => "week",
            Self::Monthly => "month",
        }
    }

    pub fn label_upper(self) -> &'static str {
        match self {
            Self::Weekly => "WEEK",
            Self::Monthly => "MONTH",
        }
    }
}

pub struct RetroSession {
    pub date: String,
    pub name: Option<String>,
    /// Normalised sport type: "Cycling", "Run", "Swim", "Walk", etc.
    pub sport_type: String,
    pub duration_mins: u32,
    pub avg_power: Option<u32>,
    pub tss: Option<f32>,
    pub kj: f32,
}

pub struct RetrospectiveContext {
    pub athlete: AthleteProfile,
    pub period: RetroPeriod,
    pub sessions: Vec<RetroSession>,
    pub wellness: Vec<WellnessSnapshot>,
    pub ctl_start: f64,
    pub ctl_end: f64,
    pub atl_end: f64,
    pub tsb_end: f64,
    pub athlete_context: String,
}

pub fn build_retrospective_prompt(ctx: &RetrospectiveContext) -> String {
    let period = ctx.period.label();
    let period_upper = ctx.period.label_upper();

    let has_cross_training = ctx.sessions.iter().any(|s| s.sport_type != "Cycling");

    let sessions_str = if ctx.sessions.is_empty() {
        format!("  No training sessions recorded this {period}.")
    } else {
        ctx.sessions
            .iter()
            .map(|s| {
                let is_cycling = s.sport_type == "Cycling";
                let name_str = s
                    .name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                let sport_tag = if !is_cycling {
                    format!(" [{}]", s.sport_type)
                } else {
                    String::new()
                };
                // Power only meaningful for cycling
                let power_str = if is_cycling {
                    s.avg_power
                        .map(|p| format!(", avg {p} W"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let tss_str = s.tss.map(|t| format!(", TSS {t:.0}")).unwrap_or_default();
                let kj_str = if is_cycling && s.kj > 0.0 {
                    format!(", {:.0} kJ", s.kj)
                } else {
                    String::new()
                };
                format!(
                    "  - {}{}{}: {} min{}{}{}",
                    s.date, name_str, sport_tag, s.duration_mins, power_str, tss_str, kj_str,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let cross_training_note = if has_cross_training {
        format!(
            "\nCROSS-TRAINING NOTE:\n\
             Sessions tagged [Run], [Swim], [Walk] etc. are non-cycling activities. \
             Acknowledge each activity for what it is — do NOT describe runs as rides. \
             Explain how each activity contributes to cycling development: \
             runs build cardiovascular base and leg strength with different muscle recruitment; \
             swims build aerobic fitness with negligible leg fatigue; \
             walks and hikes are active recovery. \
             Interpret the overall training {period} as multisport, with indoor cycling as the primary discipline.\n",
            period = period
        )
    } else {
        String::new()
    };

    let wellness_str = if ctx.wellness.is_empty() {
        format!("  No wellness data for this {period}.")
    } else {
        ctx.wellness
            .iter()
            .map(|w| {
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
                    format!("  {}: no data", w.date)
                } else {
                    format!("  {}: {}", w.date, parts.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let ctl_delta = ctx.ctl_end - ctx.ctl_start;
    let trend = if ctl_delta > 2.0 {
        format!("building (+{ctl_delta:.0})")
    } else if ctl_delta < -2.0 {
        format!("declining ({ctl_delta:.0})")
    } else {
        "stable".to_string()
    };

    let context_section = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    format!(
        r#"You are an encouraging multisport endurance coach reviewing the past {period} of training. The athlete's primary goal is indoor cycling performance, but their training includes runs, swims, and other activities — treat each activity for what it actually is, never call a run a ride. Use simple, friendly language — avoid technical jargon, and when you must use a term (like CTL or TSB), briefly explain what it means in plain words. Be warm, positive, and motivating even when pointing out things to improve.

{context_section}ATHLETE DATA: FTP {ftp} W · {weight:.1} kg · max HR {max_hr} bpm

TRAINING LOAD THIS {period_upper}:
- Fitness (CTL) at start: {ctl_start:.0} → Fitness (CTL) now: {ctl_end:.0} (trend: {trend})
- Fatigue (ATL) now: {atl:.0}
- Form (TSB = fitness minus fatigue) now: {tsb:+.0}

SESSIONS THIS {period_upper} (oldest → newest):
{sessions}
{cross_training}
WELLNESS THIS {period_upper} (oldest → newest):
{wellness}

Write a warm, encouraging retrospective using exactly this structure. Do NOT use tables — use bullet points and short paragraphs only.

1. **What Happened This {period_cap}** (Context)
   A friendly 2–3 sentence summary of the {period}'s training in plain language. Mention total sessions, how hard the athlete worked overall, and one positive highlight. Treat the reader as a motivated beginner who wants to understand their progress.

2. **What Your Body Was Telling You** (Patterns)
   2–3 bullet points explaining what the training load, wellness signals (sleep, HRV), and session data reveal. Use everyday language — e.g. "Your body was still recovering on Tuesday, which is why the effort felt harder than usual." Keep each bullet to 1–2 sentences.

3. **What to Do Next {period_cap}** (Solutions + Call to Action)
   2–3 concrete, specific, immediately actionable steps. Start each with a strong action verb ("Try…", "Add…", "Swap…", "Aim for…"). If relevant, include Turkish food suggestions for fueling or recovery (pilav, simit, yoğurt, lor peyniri, mercimek çorbası, ayran) with specific quantities. End with one short motivational sentence."#,
        period = period,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        max_hr = ctx.athlete.max_hr,
        period_upper = period_upper,
        ctl_start = ctx.ctl_start,
        ctl_end = ctx.ctl_end,
        trend = trend,
        atl = ctx.atl_end,
        tsb = ctx.tsb_end,
        sessions = sessions_str,
        cross_training = cross_training_note,
        wellness = wellness_str,
        period_cap = {
            let mut chars = period.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(sport: &str) -> RetroSession {
        RetroSession {
            date: "2026-06-10".into(),
            name: Some("Morning effort".into()),
            sport_type: sport.into(),
            duration_mins: 60,
            avg_power: Some(200),
            tss: Some(60.0),
            kj: 540.0,
        }
    }

    fn ctx(period: RetroPeriod, sessions: Vec<RetroSession>) -> RetrospectiveContext {
        RetrospectiveContext {
            athlete: AthleteProfile::default(),
            period,
            sessions,
            wellness: Vec::new(),
            ctl_start: 48.0,
            ctl_end: 52.0,
            atl_end: 50.0,
            tsb_end: 2.0,
            athlete_context: String::new(),
        }
    }

    #[test]
    fn period_labels() {
        assert_eq!(RetroPeriod::Weekly.label(), "week");
        assert_eq!(RetroPeriod::Weekly.label_upper(), "WEEK");
        assert_eq!(RetroPeriod::Monthly.label(), "month");
        assert_eq!(RetroPeriod::Monthly.label_upper(), "MONTH");
    }

    #[test]
    fn cross_training_note_present_only_with_non_cycling() {
        let with_run = build_retrospective_prompt(&ctx(RetroPeriod::Weekly, vec![session("Run")]));
        assert!(with_run.contains("CROSS-TRAINING NOTE"));

        let cycling_only =
            build_retrospective_prompt(&ctx(RetroPeriod::Weekly, vec![session("Cycling")]));
        assert!(!cycling_only.contains("CROSS-TRAINING NOTE"));
    }

    #[test]
    fn prompt_capitalises_period_in_headings() {
        let weekly =
            build_retrospective_prompt(&ctx(RetroPeriod::Weekly, vec![session("Cycling")]));
        assert!(weekly.contains("What Happened This Week"));

        let monthly =
            build_retrospective_prompt(&ctx(RetroPeriod::Monthly, vec![session("Cycling")]));
        assert!(monthly.contains("What Happened This Month"));
    }

    #[test]
    fn prompt_reports_building_trend() {
        // ctl 48 → 52 is +4 → "building"
        let prompt =
            build_retrospective_prompt(&ctx(RetroPeriod::Weekly, vec![session("Cycling")]));
        assert!(prompt.contains("building"));
    }

    #[test]
    fn empty_sessions_render_placeholder() {
        let prompt = build_retrospective_prompt(&ctx(RetroPeriod::Weekly, Vec::new()));
        assert!(prompt.contains("No training sessions recorded this week"));
    }
}
