//! The one prompt, and the markers its reply is read back with.
//!
//! The section sentinels live here rather than in [`super::parse`] because both
//! sides need them and only one of them can own them. Asking for a marker the
//! parser does not look for — or looking for one the prompt never asked for —
//! is the failure this module is shaped to make impossible, and the test at the
//! bottom is what actually holds it.

use crate::ai::coach::{RecentSession, WellnessSnapshot, WorkoutOption};
use crate::data::{athlete::AthleteProfile, db::AthleteGoal};

// ── The markers ───────────────────────────────────────────────────────────────

/// How the rider is this morning.
pub const SECTION_READINESS: &str = "===READINESS===";
/// What the training load has been doing.
pub const SECTION_FORM: &str = "===FORM===";
/// What to do about today.
pub const SECTION_SESSION: &str = "===SESSION===";
/// Pre, intra and post-ride fuelling.
pub const SECTION_FUELING: &str = "===FUELING===";

/// The line carrying how hard today should be.
pub const MARKER_VERDICT: &str = "VERDICT:";

/// The line carrying a freely chosen workout. Only ever asked for when no
/// program owns the day.
pub const MARKER_RECOMMENDED: &str = "RECOMMENDED_WORKOUT:";

// ── The context ───────────────────────────────────────────────────────────────

/// A workout the day already has, whoever put it there.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedWorkout {
    pub name: String,
    pub duration_mins: u32,
    pub tss: f32,
    pub category: String,
}

/// What the plan says about today.
///
/// The distinction that matters is not "is a workout scheduled" but "does a
/// program own the day". A program is a progression with weeks of context
/// behind it; substituting a session out of one silently drops work the plan
/// was counting on, and the plan never makes it up. So the brief may pick a
/// workout only when nothing else has a claim on the day.
#[derive(Debug, Clone, PartialEq)]
pub enum TodayPlan {
    /// A program scheduled this session. The brief may say how hard, not which.
    Programmed(PlannedWorkout),
    /// A program is running but planned nothing for today.
    ProgramRestDay,
    /// No program, but something is on the calendar.
    Scheduled(PlannedWorkout),
    /// No program and an empty day — the brief is free to choose.
    Open,
}

impl TodayPlan {
    /// Whether a program owns the day, and so whether substitution is barred.
    ///
    /// Note this is true of a program's rest day too: an empty Tuesday in a
    /// plan is a decision the plan made, not a gap to fill.
    pub fn program_active(&self) -> bool {
        matches!(self, Self::Programmed(_) | Self::ProgramRestDay)
    }

    /// The session on the day, if there is one.
    pub fn planned(&self) -> Option<&PlannedWorkout> {
        match self {
            Self::Programmed(w) | Self::Scheduled(w) => Some(w),
            Self::ProgramRestDay | Self::Open => None,
        }
    }
}

/// Everything the brief is written from.
pub struct BriefContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
    /// CTL four weeks ago, for the trend sentence.
    pub ctl_4wk_ago: f64,
    /// Weekly TSS totals, oldest to newest.
    pub week_tss: Vec<f32>,
    pub total_sessions: usize,
    /// Recent training, newest first, cycling and otherwise.
    pub recent_sessions: Vec<RecentSession>,
    /// Recent wellness, newest first. The first entry is this morning's.
    pub wellness: Vec<WellnessSnapshot>,
    pub goals: Vec<AthleteGoal>,
    pub athlete_context: String,
    pub plan: TodayPlan,
    /// What the brief may choose from, when it may choose at all.
    pub workout_options: Vec<WorkoutOption>,
    /// Upcoming dates marked as time off (`YYYY-MM-DD`).
    pub time_off_dates: Vec<String>,
    /// Today, as the rider's calendar has it.
    pub today: String,
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn format_wellness(wellness: &[WellnessSnapshot]) -> String {
    if wellness.is_empty() {
        return "  No wellness data available.".to_string();
    }
    wellness
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
                let score = w
                    .sleep_score
                    .map(|s| format!(" (score {s})"))
                    .unwrap_or_default();
                parts.push(format!("sleep {h:.1} h{score}"));
            }
            if let Some(v) = w.steps {
                parts.push(format!("steps {v}"));
            }
            if let Some(v) = w.calories {
                parts.push(format!("calories {v}"));
            }
            if parts.is_empty() {
                format!("  {}: no data", w.date)
            } else {
                format!("  {}: {}", w.date, parts.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The wellness heading, which must not claim a reading the athlete does not have.
///
/// The first line was described as "this morning" unconditionally. When the
/// overnight sync has not run — or the watch was not worn — the newest entry can
/// be days old, and the brief was then written as though a stale HRV were
/// today's: a resting HR from two days ago read as evidence the athlete is
/// recovered now. Say which it actually is, and let the coach discount it.
fn wellness_header(wellness: &[WellnessSnapshot], today: &str) -> String {
    match wellness.first() {
        // format_wellness says "no wellness data available"; no claim to make.
        None => "WELLNESS (last 7 days):".to_string(),
        Some(w) if w.date == today => {
            "WELLNESS (last 7 days, newest first — the first line is this morning):".to_string()
        }
        Some(w) => format!(
            "WELLNESS (last 7 days, newest first). There is no reading for today ({today}) \
             yet — the newest is from {date}, so treat it as that day's and not as this \
             morning's:",
            date = w.date
        ),
    }
}

fn format_sessions(sessions: &[RecentSession]) -> String {
    if sessions.is_empty() {
        return "  No sessions recorded in the last 4 weeks.".to_string();
    }
    sessions
        .iter()
        .map(|s| {
            let cycling = s.sport_type == "Cycling";
            let name = s
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();
            let sport = if cycling {
                String::new()
            } else {
                format!(" [{}]", s.sport_type)
            };
            // Power and work are bike watts or nothing — a running power meter
            // measures something else entirely.
            let power = if cycling {
                s.avg_power
                    .map(|p| format!(", avg power {p} W"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let tss = s.tss.map(|t| format!(", TSS {t:.0}")).unwrap_or_default();
            let rpe = s
                .rpe
                .map(|r| {
                    let label = match r {
                        1 => "Very Easy",
                        2 => "Easy",
                        3 => "Moderate",
                        4 => "Hard",
                        5 => "Very Hard",
                        _ => "Maximum Effort",
                    };
                    format!(", RPE {r}/6 ({label})")
                })
                .unwrap_or_default();
            // Work done, cycling only: a running power meter measures
            // something else, so its kilojoules are not comparable.
            let kj = if cycling && s.kj > 0.0 {
                format!(", {:.0} kJ", s.kj)
            } else {
                String::new()
            };
            format!(
                "  - {}{}{}: {} min{}{}{}{}",
                s.date, name, sport, s.duration_mins, power, tss, rpe, kj
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_week_tss(week_tss: &[f32]) -> String {
    if week_tss.is_empty() {
        return "  No weekly totals yet.".to_string();
    }
    week_tss
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = if i + 1 == week_tss.len() {
                "current week".to_string()
            } else {
                format!("{} week(s) ago", week_tss.len() - 1 - i)
            };
            format!("  {label}: {t:.0} TSS")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_plan(plan: &TodayPlan) -> String {
    match plan {
        TodayPlan::Programmed(w) => format!(
            "{} ({}, {} min, TSS {:.0}) — scheduled by the athlete's training program.",
            w.name, w.category, w.duration_mins, w.tss
        ),
        TodayPlan::ProgramRestDay => {
            "Nothing scheduled. The athlete is following a training program, and it \
             planned no session for today — this is a planned rest day, not a gap."
                .to_string()
        }
        TodayPlan::Scheduled(w) => format!(
            "{} ({}, {} min, TSS {:.0}) — on the calendar; no training program is running.",
            w.name, w.category, w.duration_mins, w.tss
        ),
        TodayPlan::Open => "Nothing scheduled, and no training program is running.".to_string(),
    }
}

/// The instructions for the SESSION section, which are the whole precedence
/// rule in prose.
fn session_instructions(plan: &TodayPlan, options: &str) -> String {
    match plan {
        TodayPlan::Programmed(w) => format!(
            "The athlete's training program has already chosen today's session: {name}. \
             Do NOT suggest a different workout — the program is a multi-week progression, \
             and swapping a session out of it drops work the plan was counting on. \
             Your job is to say whether today's rider should do it as written, do it \
             lighter, or not train at all. Explain which and why in two or three sentences.",
            name = w.name
        ),
        TodayPlan::ProgramRestDay => {
            "The athlete's training program planned no session today. Confirm that resting \
             is right, or note briefly if their signals suggest they could add something \
             easy. Do NOT prescribe a workout from any list."
                .to_string()
        }
        TodayPlan::Scheduled(w) => format!(
            "Today's calendar has {name}. No training program is running, so you may keep it \
             or swap it for a better fit from this library:\n{options}\n\
             If you swap, name the replacement exactly as it appears above.",
            name = w.name,
            options = options
        ),
        TodayPlan::Open => format!(
            "The day is open and no training program is running, so choose one session from \
             this library — or none, if resting is the right call:\n{options}\n\
             Name it exactly as it appears above.",
            options = options
        ),
    }
}

// ── The prompt ────────────────────────────────────────────────────────────────

/// Build the one request behind every AI card in the app.
pub fn build_brief_prompt(ctx: &BriefContext) -> String {
    // The same thresholds the Fitness page shows the rider. This table used to
    // run 15 points fresher than the page, so a brief could call a form of +3
    // "good form" while the page beside it called the same number fatigue.
    let tsb_desc = crate::training::fitness::TsbBand::of(ctx.tsb).prompt_description();

    let ctl_delta = ctx.ctl - ctx.ctl_4wk_ago;
    let trend = if ctl_delta > 3.0 {
        format!("improving — CTL up {ctl_delta:.0} points over 4 weeks")
    } else if ctl_delta < -3.0 {
        format!(
            "declining — CTL down {:.0} points over 4 weeks",
            ctl_delta.abs()
        )
    } else {
        "stable over the past 4 weeks".to_string()
    };

    let wkg = if ctx.athlete.weight_kg > 0.0 {
        format!(
            "{:.2}",
            ctx.athlete.ftp_watts as f32 / ctx.athlete.weight_kg
        )
    } else {
        "unknown".to_string()
    };

    let background = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    let goals = if ctx.goals.is_empty() {
        "  No goals specified.".to_string()
    } else {
        ctx.goals
            .iter()
            .map(|g| format!("  - {}", g.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // The planned session must not appear in its own list of alternatives.
    // Offered both as the plan and as the replacement, a model will sometimes
    // pick it, producing "swap Endurance 60 for Endurance 60".
    let planned_name = ctx.plan.planned().map(|w| w.name.as_str());
    let alternatives: Vec<&WorkoutOption> = ctx
        .workout_options
        .iter()
        .filter(|w| match planned_name {
            Some(planned) => !crate::ai::naming::names_match(&w.name, planned),
            None => true,
        })
        .collect();
    let options = if alternatives.is_empty() {
        "  No workouts in the library.".to_string()
    } else {
        alternatives
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

    let cross_training = if ctx
        .recent_sessions
        .iter()
        .any(|s| s.sport_type != "Cycling")
    {
        "\nCROSS-TRAINING CONTEXT:\n\
         Sessions tagged [Run], [Swim], [Walk], etc. are non-cycling activities. \
         Interpret their impact on cycling readiness as follows:\n\
         - Run / Virtual Run: aerobic cross-training sharing the cardiovascular system. \
           Running fatigue is partly separate from cycling fatigue (different primary muscles), \
           but a hard run still raises ATL and warrants slightly reduced cycling intensity the next day.\n\
         - Walk / Hike: low-intensity active recovery — negligible cycling impact.\n\
         - Swim: low-impact aerobic work — cardiovascular base with minimal leg fatigue.\n\
         - Strength Training: neuromuscular stimulus that may cause delayed leg soreness; \
           allow 24–48 h before a hard cycling session.\n\
         - Other cross-training: treat as general aerobic load contributing to CTL.\n"
    } else {
        ""
    };

    let is_today_off = ctx.time_off_dates.contains(&ctx.today);
    let time_off = if ctx.time_off_dates.is_empty() {
        String::new()
    } else if is_today_off {
        let future: Vec<&String> = ctx
            .time_off_dates
            .iter()
            .filter(|d| *d != &ctx.today)
            .collect();
        let mut s = "\nTODAY IS A SCHEDULED TIME OFF DAY — no indoor cycling. \
                     In the SESSION section, prescribe a non-cycling activity \
                     (a walk, yoga, an easy swim, stretching) instead of a workout, \
                     and answer VERDICT: REST."
            .to_string();
        if !future.is_empty() {
            s.push_str(&format!(
                "\n\nFurther time off (no cycling on these days):\n{}",
                future
                    .iter()
                    .map(|d| format!("  - {d}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        s
    } else {
        format!(
            "\nUPCOMING TIME OFF (no indoor cycling on these dates):\n{}\n",
            ctx.time_off_dates
                .iter()
                .map(|d| format!("  - {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    // Fuelling only makes sense around a session that is actually happening.
    let fueling_instructions = match (ctx.plan.planned(), is_today_off) {
        (Some(w), false) => format!(
            "Pre, intra and post-ride fuelling for a {dur}-minute session at TSS {tss:.0}. \
             Give carbohydrate grams, protein grams and fluid volumes with timing. \
             Suggest specific Turkish foods and portions:\n\
             \x20 Pre: pilav, makarna, ekmek, simit, yulaf ezmesi\n\
             \x20 Intra (only if over 60 minutes): banana, hurma, portakal suyu, kuru üzüm\n\
             \x20 Post: yoğurt, lor peyniri, beyaz peynir, yumurta, mercimek çorbası\n\
             \x20 Fluids: ayran, şalgam suyu, limonata with a pinch of salt",
            dur = w.duration_mins,
            tss = w.tss
        ),
        _ => "No session is planned, so give one short line on eating for recovery today \
              — Turkish foods, no tables, no fuelling schedule."
            .to_string(),
    };

    let session_instructions = session_instructions(&ctx.plan, &options);

    format!(
        r#"You are an expert endurance coach and sports nutritionist writing one athlete's morning brief. Their primary sport is indoor cycling, but their training also includes runs, swims and other activities — CTL and ATL reflect all sport types.

{background}ATHLETE: FTP {ftp} W · {weight:.1} kg · {wkg} W/kg · max HR {max_hr} bpm

TRAINING STATUS (as of {today}):
- CTL (fitness, 42-day EMA): {ctl:.0}
- ATL (fatigue, 7-day EMA): {atl:.0}
- TSB (form, CTL − ATL): {tsb:+.0} — {tsb_desc}
- Fitness trend: {trend}
- Sessions recorded all time: {total_sessions}

WEEKLY TSS (oldest → newest):
{week_tss}

{wellness_header}
{wellness}

RECENT TRAINING (last 4 weeks, newest first):
{sessions}
{cross_training}{time_off}
ATHLETE GOALS:
{goals}

TODAY'S PLAN:
{plan}

Write the brief in exactly four sections, each opening with its marker on its own line. Use the markers verbatim. Do not add sections, and do not number them.

{s_readiness}
Two or three sentences reading form and this morning's wellness together into one picture of how this athlete is today. Lead with the conclusion.

{s_form}
Three or four sentences on what the training load is doing: what the CTL/ATL/TSB relationship means right now, whether fitness is building, holding or declining, and whether that pace is sustainable. Mention recovery signals from HRV, resting HR and sleep only if there is wellness data. Do not prescribe a workout here.

{s_session}
{session_instructions}

{s_fueling}
{fueling_instructions}

Under 450 words in total. Write plainly and address the athlete directly — this is read before breakfast. Use markdown for emphasis and lists, but no tables and no headings of your own.

Then end with exactly one line:
{v} PROCEED
{v} EASE
{v} REST

PROCEED means today's plan suits the athlete as it stands. EASE means train, but lighter than planned. REST means do not train today.{recommend}"#,
        background = background,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        wkg = wkg,
        max_hr = ctx.athlete.max_hr,
        today = ctx.today,
        ctl = ctx.ctl,
        atl = ctx.atl,
        tsb = ctx.tsb,
        tsb_desc = tsb_desc,
        trend = trend,
        total_sessions = ctx.total_sessions,
        week_tss = format_week_tss(&ctx.week_tss),
        wellness_header = wellness_header(&ctx.wellness, &ctx.today),
        wellness = format_wellness(&ctx.wellness),
        sessions = format_sessions(&ctx.recent_sessions),
        cross_training = cross_training,
        time_off = time_off,
        goals = goals,
        plan = describe_plan(&ctx.plan),
        s_readiness = SECTION_READINESS,
        s_form = SECTION_FORM,
        s_session = SECTION_SESSION,
        s_fueling = SECTION_FUELING,
        session_instructions = session_instructions,
        fueling_instructions = fueling_instructions,
        v = MARKER_VERDICT,
        // Only offered when the brief is allowed to choose. Asking for a
        // workout name and then discarding it wastes tokens and invites the
        // model to argue for a session the athlete will never be shown.
        recommend = if ctx.plan.program_active() {
            String::new()
        } else {
            format!(
                "\n\nIf you are recommending a workout from the library, add one more line:\n\
                 {MARKER_RECOMMENDED} <exact workout name>"
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(name: &str) -> PlannedWorkout {
        PlannedWorkout {
            name: name.into(),
            duration_mins: 60,
            tss: 75.0,
            category: "Threshold".into(),
        }
    }

    fn option(name: &str) -> WorkoutOption {
        WorkoutOption {
            name: name.into(),
            duration_mins: 60,
            tss: 60.0,
            category: "Endurance".into(),
        }
    }

    fn wellness_on(date: &str) -> WellnessSnapshot {
        WellnessSnapshot {
            date: date.into(),
            hrv: Some(31.0),
            resting_hr: Some(55),
            sleep_hours: Some(6.9),
            sleep_score: Some(37),
            steps: Some(162),
            calories: None,
        }
    }

    #[test]
    fn should_call_the_first_line_this_morning_when_it_is_todays_reading() {
        let mut c = ctx();
        c.wellness = vec![wellness_on("2026-08-11")];
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("the first line is this morning"));
    }

    #[test]
    fn should_date_the_newest_reading_when_today_has_none() {
        // The bug this replaces: a reading from two days ago was announced as
        // "this morning", so a stale HRV read as evidence of recovery now.
        let mut c = ctx();
        c.wellness = vec![wellness_on("2026-08-09")];
        let prompt = build_brief_prompt(&c);
        assert!(!prompt.contains("the first line is this morning"));
        assert!(prompt.contains("no reading for today (2026-08-11)"));
        assert!(prompt.contains("the newest is from 2026-08-09"));
    }

    #[test]
    fn should_claim_nothing_about_this_morning_when_there_is_no_wellness_at_all() {
        // "this morning" still appears in the READINESS instructions; what must
        // not appear is the claim that a wellness line holds today's reading.
        let c = ctx();
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("WELLNESS (last 7 days):"));
        assert!(!prompt.contains("the first line is this morning"));
    }

    fn ctx() -> BriefContext {
        BriefContext {
            athlete: AthleteProfile::default(),
            ctl: 50.0,
            atl: 45.0,
            tsb: 5.0,
            ctl_4wk_ago: 45.0,
            week_tss: vec![300.0, 350.0],
            total_sessions: 120,
            recent_sessions: Vec::new(),
            wellness: Vec::new(),
            goals: Vec::new(),
            athlete_context: String::new(),
            plan: TodayPlan::Open,
            workout_options: Vec::new(),
            time_off_dates: Vec::new(),
            today: "2026-08-11".into(),
        }
    }

    // ── The prompt/parser seam ───────────────────────────────────────────────

    #[test]
    fn should_ask_for_every_section_marker() {
        // The parser looks for exactly these. A prompt that stops asking for
        // one produces a card that is silently always empty.
        let prompt = build_brief_prompt(&ctx());
        for marker in [
            SECTION_READINESS,
            SECTION_FORM,
            SECTION_SESSION,
            SECTION_FUELING,
        ] {
            assert!(prompt.contains(marker), "prompt never asks for {marker}");
        }
        assert!(prompt.contains(MARKER_VERDICT));
    }

    #[test]
    fn should_ask_for_each_verdict_the_parser_understands() {
        let prompt = build_brief_prompt(&ctx());
        for verdict in ["PROCEED", "EASE", "REST"] {
            assert!(
                prompt.contains(&format!("{MARKER_VERDICT} {verdict}")),
                "prompt never offers {verdict}"
            );
        }
    }

    // ── Precedence ───────────────────────────────────────────────────────────

    #[test]
    fn should_forbid_substitution_when_a_program_owns_the_day() {
        let mut c = ctx();
        c.plan = TodayPlan::Programmed(planned("Threshold 3x12"));
        c.workout_options = vec![option("Recovery Spin")];
        let prompt = build_brief_prompt(&c);

        assert!(prompt.contains("Do NOT suggest a different workout"));
        assert!(
            !prompt.contains(MARKER_RECOMMENDED),
            "a name that would only be discarded must not be asked for"
        );
        assert!(
            !prompt.contains("- Recovery Spin"),
            "the library is not offered when it may not be chosen from"
        );
    }

    #[test]
    fn should_forbid_substitution_on_a_programs_rest_day() {
        // An empty Tuesday inside a plan is a decision, not a gap to fill.
        let mut c = ctx();
        c.plan = TodayPlan::ProgramRestDay;
        c.workout_options = vec![option("Recovery Spin")];
        let prompt = build_brief_prompt(&c);

        assert!(prompt.contains("planned no session today"));
        assert!(prompt.contains("Do NOT prescribe a workout"));
        assert!(!prompt.contains(MARKER_RECOMMENDED));
    }

    #[test]
    fn should_offer_the_library_when_no_program_is_running() {
        let mut c = ctx();
        c.workout_options = vec![option("Recovery Spin")];
        let prompt = build_brief_prompt(&c);

        assert!(prompt.contains("- Recovery Spin"));
        assert!(prompt.contains(MARKER_RECOMMENDED));
    }

    #[test]
    fn should_let_a_calendar_entry_be_swapped_when_no_program_is_running() {
        let mut c = ctx();
        c.plan = TodayPlan::Scheduled(planned("Threshold 3x12"));
        c.workout_options = vec![option("Recovery Spin")];
        let prompt = build_brief_prompt(&c);

        assert!(prompt.contains("no training program is running"));
        assert!(prompt.contains(MARKER_RECOMMENDED));
    }

    #[test]
    fn should_not_offer_the_planned_workout_as_its_own_alternative() {
        // Offered as both the plan and the replacement, a model will sometimes
        // pick it — "swap Endurance 60 for Endurance 60".
        let mut c = ctx();
        c.plan = TodayPlan::Scheduled(planned("Endurance 60"));
        c.workout_options = vec![option("Endurance 60"), option("Recovery Spin")];
        let prompt = build_brief_prompt(&c);

        assert!(prompt.contains("- Recovery Spin"));
        assert!(!prompt.contains("- Endurance 60 ("));
    }

    #[test]
    fn should_say_so_when_the_library_holds_only_the_planned_workout() {
        let mut c = ctx();
        c.plan = TodayPlan::Scheduled(planned("Endurance 60"));
        c.workout_options = vec![option("Endurance 60")];
        assert!(build_brief_prompt(&c).contains("No workouts in the library."));
    }

    // ── Time off and fuelling ────────────────────────────────────────────────

    #[test]
    fn should_flag_today_as_a_time_off_day() {
        let mut c = ctx();
        c.time_off_dates = vec!["2026-08-11".into()];
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("TODAY IS A SCHEDULED TIME OFF DAY"));
        assert!(prompt.contains("VERDICT: REST"));
    }

    #[test]
    fn should_list_upcoming_time_off_without_flagging_today() {
        let mut c = ctx();
        c.time_off_dates = vec!["2026-08-20".into()];
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("UPCOMING TIME OFF"));
        assert!(!prompt.contains("TODAY IS A SCHEDULED TIME OFF DAY"));
    }

    #[test]
    fn should_ask_for_full_fuelling_only_when_a_session_is_planned() {
        let mut c = ctx();
        assert!(!build_brief_prompt(&c).contains("Pre, intra and post-ride fuelling"));

        c.plan = TodayPlan::Programmed(planned("Threshold 3x12"));
        assert!(build_brief_prompt(&c).contains("Pre, intra and post-ride fuelling"));
    }

    #[test]
    fn should_not_ask_for_workout_fuelling_on_a_time_off_day() {
        // A session on the calendar the athlete is not going to ride.
        let mut c = ctx();
        c.plan = TodayPlan::Scheduled(planned("Threshold 3x12"));
        c.time_off_dates = vec!["2026-08-11".into()];
        assert!(!build_brief_prompt(&c).contains("Pre, intra and post-ride fuelling"));
    }

    // ── Shared framing ───────────────────────────────────────────────────────

    #[test]
    fn should_describe_form_the_way_the_fitness_page_does() {
        // The brief and the page beside it must not disagree about what a
        // given TSB means.
        let mut c = ctx();
        c.tsb = -22.0;
        let expected = crate::training::fitness::TsbBand::of(-22.0).prompt_description();
        assert!(build_brief_prompt(&c).contains(expected));
    }

    #[test]
    fn should_describe_which_way_fitness_is_moving() {
        let mut c = ctx();
        c.ctl = 60.0;
        c.ctl_4wk_ago = 50.0;
        assert!(build_brief_prompt(&c).contains("improving"));

        c.ctl_4wk_ago = 70.0;
        assert!(build_brief_prompt(&c).contains("declining"));

        // Inside the band either way is neither, and saying so beats implying
        // a trend from noise.
        c.ctl_4wk_ago = 61.0;
        assert!(build_brief_prompt(&c).contains("stable"));
    }

    #[test]
    fn should_carry_the_athletes_ftp_and_the_library_it_may_choose_from() {
        let mut c = ctx();
        c.workout_options = vec![option("Endurance 60")];
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("FTP 200 W"));
        assert!(prompt.contains("Endurance 60"));
    }

    #[test]
    fn should_include_the_athletes_own_background_when_they_wrote_one() {
        let mut c = ctx();
        assert!(!build_brief_prompt(&c).contains("ATHLETE BACKGROUND"));

        c.athlete_context = "Coming back from a knee injury.".into();
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("ATHLETE BACKGROUND"));
        assert!(prompt.contains("Coming back from a knee injury."));
    }

    #[test]
    fn should_explain_cross_training_only_when_there_is_some() {
        let mut c = ctx();
        assert!(!build_brief_prompt(&c).contains("CROSS-TRAINING CONTEXT"));

        c.recent_sessions = vec![RecentSession {
            date: "2026-08-10".into(),
            name: Some("Morning run".into()),
            sport_type: "Run".into(),
            duration_mins: 45,
            avg_power: Some(280),
            tss: Some(50.0),
            kj: 0.0,
            rpe: None,
        }];
        let prompt = build_brief_prompt(&c);
        assert!(prompt.contains("CROSS-TRAINING CONTEXT"));
        assert!(prompt.contains("[Run]"));
        assert!(
            !prompt.contains("280 W"),
            "running power is not bike watts and must not be shown as though it were"
        );
    }

    // ── TodayPlan ────────────────────────────────────────────────────────────

    #[test]
    fn should_treat_a_programs_rest_day_as_the_program_owning_the_day() {
        assert!(TodayPlan::ProgramRestDay.program_active());
        assert!(TodayPlan::Programmed(planned("X")).program_active());
        assert!(!TodayPlan::Scheduled(planned("X")).program_active());
        assert!(!TodayPlan::Open.program_active());
    }

    #[test]
    fn should_report_the_session_on_the_day_whoever_scheduled_it() {
        assert_eq!(
            TodayPlan::Programmed(planned("X"))
                .planned()
                .map(|w| &*w.name),
            Some("X")
        );
        assert_eq!(
            TodayPlan::Scheduled(planned("Y"))
                .planned()
                .map(|w| &*w.name),
            Some("Y")
        );
        assert_eq!(TodayPlan::ProgramRestDay.planned(), None);
        assert_eq!(TodayPlan::Open.planned(), None);
    }
}
