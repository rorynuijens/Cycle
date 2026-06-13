use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::data::{athlete::AthleteProfile, db::AthleteGoal};

// ── Workout options (library → prompt) ───────────────────────────────────────

pub struct WorkoutOption {
    pub name: String,
    pub duration_mins: u32,
    pub tss: f32,
    pub category: String,
}

// ── Wellness snapshot (shared by both Fitness and Coaching contexts) ──────────

pub struct WellnessSnapshot {
    pub date: String,
    pub hrv: Option<f32>,
    pub resting_hr: Option<u32>,
    pub sleep_hours: Option<f32>,
    pub sleep_score: Option<u32>,
    pub steps: Option<u32>,
    pub calories: Option<u32>,
}

// ── Fitness insight (Fitness tab) ─────────────────────────────────────────────

pub struct FitnessContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
    /// CTL value 4 weeks ago — used to describe trend direction.
    pub ctl_4wk_ago: f64,
    /// Weekly TSS totals, oldest-to-newest, up to 6 entries.
    pub week_tss: Vec<f32>,
    pub total_sessions: usize,
    pub athlete_context: String,
    /// Recent wellness entries, newest first (up to 7).
    pub wellness: Vec<WellnessSnapshot>,
}

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
                let score_str = w
                    .sleep_score
                    .map(|s| format!(" (score {s})"))
                    .unwrap_or_default();
                parts.push(format!("sleep {h:.1} h{score_str}"));
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

pub fn build_fitness_prompt(ctx: &FitnessContext) -> String {
    let ctl_delta = ctx.ctl - ctx.ctl_4wk_ago;
    let trend = if ctl_delta > 3.0 {
        format!("improving — CTL up {ctl_delta:.0} points over 4 weeks")
    } else if ctl_delta < -3.0 {
        let drop = ctl_delta.abs();
        format!("declining — CTL down {drop:.0} points over 4 weeks")
    } else {
        "stable over the past 4 weeks".to_string()
    };

    let week_tss_str = ctx
        .week_tss
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = if i + 1 == ctx.week_tss.len() {
                "current week".to_string()
            } else {
                format!("{} week(s) ago", ctx.week_tss.len() - 1 - i)
            };
            format!("  {label}: {t:.0} TSS")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let wkg = if ctx.athlete.weight_kg > 0.0 {
        format!(
            "{:.2}",
            ctx.athlete.ftp_watts as f32 / ctx.athlete.weight_kg
        )
    } else {
        "unknown".to_string()
    };

    let context_section = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    let wellness_str = format_wellness(&ctx.wellness);

    format!(
        r#"You are an exercise physiologist specialising in endurance sports. The athlete's primary discipline is indoor cycling, but their training load (CTL/ATL) includes all sport types — running, swimming, etc. Analyse the training load and wellness data accordingly.
Provide a brief, plain-language interpretation.

{context_section}ATHLETE: FTP {ftp} W · {weight:.1} kg · {wkg} W/kg · max HR {max_hr} bpm

CURRENT TRAINING METRICS:
- CTL (fitness, 42-day EMA): {ctl:.0}
- ATL (fatigue, 7-day EMA): {atl:.0}
- TSB (form = CTL − ATL): {tsb:+.0}
- Fitness trend: {trend}

WEEKLY TSS (oldest → newest):
{week_tss}

Total sessions recorded: {sessions}

WELLNESS (last 7 days, newest first):
{wellness}

Provide a focused interpretation (max 250 words) with these headings:
1. **Current state**: What the CTL/ATL/TSB relationship means right now in plain language
2. **Trend**: Whether fitness is building, stable, or declining — and whether the pace is sustainable
3. **Recovery signals**: What the HRV, resting HR, and sleep data indicate about current recovery status — only include if wellness data is available
4. **Watch out** (optional): Only include if there is a genuine concern such as overreaching, poor sleep trend, or very high monotony — skip entirely if everything looks fine

Do not suggest specific workouts. Focus purely on interpreting what the numbers mean."#,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        wkg = wkg,
        max_hr = ctx.athlete.max_hr,
        ctl = ctx.ctl,
        atl = ctx.atl,
        tsb = ctx.tsb,
        trend = trend,
        week_tss = week_tss_str,
        sessions = ctx.total_sessions,
        wellness = wellness_str,
    )
}

// ── Workout suggestion (Coaching tab) ────────────────────────────────────────

pub struct TrainingContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub atl: f64,
    pub tsb: f64,
    pub recent_sessions: Vec<RecentSession>,
    pub goals: Vec<AthleteGoal>,
    pub athlete_context: String,
    pub workout_options: Vec<WorkoutOption>,
    /// Recent wellness entries, newest first (up to 7).
    pub wellness: Vec<WellnessSnapshot>,
    /// Upcoming dates marked as time off (YYYY-MM-DD strings, next 14 days).
    pub time_off_dates: Vec<String>,
}

pub struct RecentSession {
    pub date: String,
    pub name: Option<String>,
    /// Normalised sport type, e.g. "Cycling", "Run", "Swim", "Walk", "Hike", "Strength Training".
    pub sport_type: String,
    pub duration_mins: u32,
    pub avg_power: Option<u32>,
    pub tss: Option<f32>,
    pub kj: f32,
    pub rpe: Option<u8>,
}

pub fn build_prompt(ctx: &TrainingContext) -> String {
    let form_desc = if ctx.tsb > 25.0 {
        "very fresh (risk of detraining if sustained)"
    } else if ctx.tsb > 5.0 {
        "optimal form — good time for quality work"
    } else if ctx.tsb > -10.0 {
        "moderate fatigue — normal training range"
    } else {
        "significant fatigue — recovery priority"
    };

    let has_cross_training = ctx
        .recent_sessions
        .iter()
        .any(|s| s.sport_type != "Cycling");

    let sessions_text = if ctx.recent_sessions.is_empty() {
        "  No sessions recorded in the last 4 weeks.".to_string()
    } else {
        ctx.recent_sessions
            .iter()
            .map(|s| {
                let is_cycling = s.sport_type == "Cycling";
                let name_str = s
                    .name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                // Tag non-cycling sport type so the AI can distinguish activity types
                let sport_tag = if !is_cycling {
                    format!(" [{}]", s.sport_type)
                } else {
                    String::new()
                };
                // Power is only meaningful for cycling; suppress it for other sports
                let power_str = if is_cycling {
                    s.avg_power
                        .map(|p| format!(", avg power {p} W"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let tss_str = s.tss.map(|t| format!(", TSS {t:.0}")).unwrap_or_default();
                let rpe_str = s
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
                // kJ only shown for cycling (running kJ uses running power, not comparable)
                let kj_str = if is_cycling && s.kj > 0.0 {
                    format!(", {:.0} kJ", s.kj)
                } else {
                    String::new()
                };
                format!(
                    "  - {}{}{}: {} min{}{}{}{}",
                    s.date,
                    name_str,
                    sport_tag,
                    s.duration_mins,
                    power_str,
                    tss_str,
                    rpe_str,
                    kj_str,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let cross_training_section = if has_cross_training {
        "\nCROSS-TRAINING CONTEXT:\n\
         Sessions tagged [Run], [Swim], [Walk], etc. are non-cycling activities. \
         Interpret their impact on cycling readiness as follows:\n\
         - Run / Virtual Run: aerobic cross-training that shares the cardiovascular system. \
           Running fatigue is partly separate from cycling fatigue (different primary muscles), \
           but a hard run still raises ATL and warrants slightly reduced cycling intensity the next day.\n\
         - Walk / Hike: low-intensity active recovery — negligible cycling impact.\n\
         - Swim: low-impact aerobic work — builds cardiovascular base with minimal leg fatigue; \
           generally compatible with normal cycling load.\n\
         - Strength Training: neuromuscular stimulus that may cause delayed leg soreness; \
           allow 24–48 h before a hard cycling session.\n\
         - Other cross-training: treat as general aerobic load contributing to CTL.\n"
    } else {
        ""
    };

    let goals_text = if ctx.goals.is_empty() {
        "  No goals specified.".to_string()
    } else {
        ctx.goals
            .iter()
            .map(|g| format!("  - {}", g.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let workout_list = if ctx.workout_options.is_empty() {
        "  No workout library available.".to_string()
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

    let wkg = if ctx.athlete.weight_kg > 0.0 {
        format!(
            "{:.2}",
            ctx.athlete.ftp_watts as f32 / ctx.athlete.weight_kg
        )
    } else {
        "unknown".to_string()
    };

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let context_section = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    let wellness_str = format_wellness(&ctx.wellness);

    let time_off_section = if ctx.time_off_dates.is_empty() {
        String::new()
    } else {
        format!(
            "\nTIME OFF SCHEDULED (no indoor cycling on these days — suggest alternative activities such as running, yoga, or pilates instead):\n{}\n",
            ctx.time_off_dates
                .iter()
                .map(|d| format!("  - {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    format!(
        r#"You are an expert cycling coach. Based on the athlete data below, provide specific, actionable training advice.

{context_section}ATHLETE PROFILE:
- FTP: {ftp} W
- Weight: {weight:.1} kg
- W/kg (FTP): {wkg}
- Max HR: {max_hr} bpm

CURRENT TRAINING LOAD (as of {today}):
- CTL (Chronic Training Load / fitness): {ctl:.0}
- ATL (Acute Training Load / fatigue): {atl:.0}
- TSB (Training Stress Balance / form): {tsb:+.0}
- Form: {form}

RECENT SESSIONS (last 4 weeks, newest first):
{sessions}
{cross_training}
WELLNESS (last 7 days, newest first):
{wellness}
{time_off}
ATHLETE GOALS:
{goals}

AVAILABLE WORKOUTS FROM THE LIBRARY:
{workouts}

Please provide:
1. **Today's workout suggestion**: Choose ONE workout from the library above that best fits the current load, form, and recovery signals. Mention the exact workout name as it appears in the list.
2. **Rationale**: one or two sentences explaining why this workout fits — reference wellness signals (HRV, sleep) if they influenced the choice. If recent non-cycling activity influenced the recommendation, explain how.
3. **Recovery note** (only if TSB is below −10 or HRV/sleep signals suggest fatigue): one practical recovery tip.

Keep the response under 300 words. Use the headings above.

End your response with exactly this line (use the exact workout name from the list above):
RECOMMENDED_WORKOUT: <exact workout name>"#,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        wkg = wkg,
        max_hr = ctx.athlete.max_hr,
        today = today,
        ctl = ctx.ctl,
        atl = ctx.atl,
        tsb = ctx.tsb,
        form = form_desc,
        sessions = sessions_text,
        cross_training = cross_training_section,
        wellness = wellness_str,
        time_off = time_off_section,
        goals = goals_text,
        workouts = workout_list,
    )
}

// ── Training program builder ──────────────────────────────────────────────────

pub struct ProgramContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub tsb: f64,
    pub goals: Vec<AthleteGoal>,
    pub athlete_context: String,
    pub workout_options: Vec<WorkoutOption>,
    pub training_days: Vec<String>,
    /// None = generate at least 8 weeks.
    pub num_weeks: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ProgramEntry {
    pub week: u32,
    pub day: String,
    pub workout_name: String,
}

pub fn build_program_prompt(ctx: &ProgramContext) -> String {
    let wkg = if ctx.athlete.weight_kg > 0.0 {
        format!(
            "{:.2}",
            ctx.athlete.ftp_watts as f32 / ctx.athlete.weight_kg
        )
    } else {
        "unknown".to_string()
    };

    let goals_text = if ctx.goals.is_empty() {
        "  No specific goals stated.".to_string()
    } else {
        ctx.goals
            .iter()
            .map(|g| format!("  - {}", g.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let context_section = if ctx.athlete_context.trim().is_empty() {
        String::new()
    } else {
        format!("ATHLETE BACKGROUND:\n{}\n\n", ctx.athlete_context.trim())
    };

    let workout_list = ctx
        .workout_options
        .iter()
        .map(|w| {
            format!(
                "  - {} ({}, {} min, TSS {:.0})",
                w.name, w.category, w.duration_mins, w.tss
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let days_str = ctx.training_days.join(", ");
    let (duration_str, week_count) = match ctx.num_weeks {
        Some(w) => (format!("{} weeks", w), w),
        None => ("open-ended".to_string(), 8),
    };

    format!(
        r#"You are an expert cycling coach building a structured training program.

{context_section}ATHLETE:
- FTP: {ftp} W · {weight:.1} kg · {wkg} W/kg
- Current CTL (fitness): {ctl:.0}
- Current TSB (form): {tsb:+.0}

GOALS:
{goals}

TRAINING SCHEDULE:
- Training days: {days}
- Program duration: {duration}

AVAILABLE WORKOUTS:
{workouts}

Build a {weeks}-week training program. Apply progressive overload: weeks 1–3 build load, week 4 is a recovery week (lighter workouts), then repeat. Match intensity to phase (recovery weeks: recovery/endurance only; build weeks: mix of sweet spot, threshold, VO₂max depending on goals and current fitness).

Return ONLY a JSON array — no text before or after it — in exactly this format:
[
  {{"week": 1, "day": "monday", "workout": "Exact Workout Name"}},
  {{"week": 1, "day": "wednesday", "workout": "Exact Workout Name"}}
]

Rules:
- Use only workout names that appear exactly in the AVAILABLE WORKOUTS list above.
- Use only the days listed in TRAINING SCHEDULE.
- Day values must be lowercase full day names: monday, tuesday, wednesday, thursday, friday, saturday, sunday."#,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        wkg = wkg,
        ctl = ctx.ctl,
        tsb = ctx.tsb,
        goals = goals_text,
        days = days_str,
        duration = duration_str,
        workouts = workout_list,
        weeks = week_count,
    )
}

/// Extract `ProgramEntry` items from Claude's JSON response.
pub fn parse_program_response(text: &str) -> Vec<ProgramEntry> {
    let start = match text.find('[') {
        Some(i) => i,
        None => return vec![],
    };
    let end = match text.rfind(']') {
        Some(i) => i + 1,
        None => return vec![],
    };
    let json_str = &text[start..end];

    #[derive(Deserialize)]
    struct RawEntry {
        week: u32,
        day: String,
        workout: String,
    }

    match serde_json::from_str::<Vec<RawEntry>>(json_str) {
        Ok(raw) => raw
            .into_iter()
            .map(|r| ProgramEntry {
                week: r.week,
                day: r.day.to_lowercase(),
                workout_name: r.workout,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to parse program JSON: {e}");
            vec![]
        }
    }
}

// ── HTTP client ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ClaudeRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ClaudeMessage<'a>>,
}

#[derive(Serialize)]
struct ClaudeMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

const CLAUDE_MODEL: &str = "claude-sonnet-4-6";

/// POST to the Claude API and return the text response.
pub async fn get_suggestion(api_key: &str, prompt: &str, max_tokens: u32) -> Result<String> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?;

    let body = ClaudeRequest {
        model: CLAUDE_MODEL,
        max_tokens,
        messages: vec![ClaudeMessage {
            role: "user",
            content: prompt,
        }],
    };

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        anyhow::bail!("API error {status}: {body_text}");
    }

    let parsed: ClaudeResponse = response
        .json()
        .await
        .context("failed to parse API response")?;

    parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .and_then(|b| b.text)
        .context("no text content in API response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(desc: &str) -> AthleteGoal {
        AthleteGoal {
            id: 1,
            description: desc.to_string(),
        }
    }

    fn workout(name: &str) -> WorkoutOption {
        WorkoutOption {
            name: name.to_string(),
            duration_mins: 60,
            tss: 60.0,
            category: "Threshold".to_string(),
        }
    }

    // ── parse_program_response ───────────────────────────────────────────────────

    #[test]
    fn parses_clean_json_array() {
        let text = r#"[
            {"week": 1, "day": "Monday", "workout": "Sweet Spot 2x20"},
            {"week": 1, "day": "WEDNESDAY", "workout": "VO2 5x3"}
        ]"#;
        let entries = parse_program_response(text);
        assert_eq!(entries.len(), 2);
        // Day names are normalised to lowercase.
        assert_eq!(entries[0].day, "monday");
        assert_eq!(entries[1].day, "wednesday");
        assert_eq!(entries[0].workout_name, "Sweet Spot 2x20");
    }

    #[test]
    fn extracts_json_embedded_in_prose() {
        // The model sometimes wraps the array in explanatory text.
        let text =
            "Here is your plan:\n[{\"week\": 2, \"day\": \"friday\", \"workout\": \"Endurance 90\"}]\nEnjoy!";
        let entries = parse_program_response(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].week, 2);
        assert_eq!(entries[0].workout_name, "Endurance 90");
    }

    #[test]
    fn returns_empty_when_no_array_present() {
        assert!(parse_program_response("Sorry, I cannot help with that.").is_empty());
    }

    #[test]
    fn returns_empty_on_malformed_json() {
        assert!(parse_program_response("[{\"week\": \"oops\"}]").is_empty());
    }

    // ── build_program_prompt ─────────────────────────────────────────────────────

    fn program_ctx(num_weeks: Option<u32>) -> ProgramContext {
        ProgramContext {
            athlete: AthleteProfile::default(),
            ctl: 50.0,
            tsb: 5.0,
            goals: vec![goal("Ride a century")],
            athlete_context: String::new(),
            workout_options: vec![workout("Sweet Spot 2x20")],
            training_days: vec!["monday".into(), "wednesday".into()],
            num_weeks,
        }
    }

    #[test]
    fn program_prompt_defaults_to_eight_weeks_when_open_ended() {
        let prompt = build_program_prompt(&program_ctx(None));
        assert!(prompt.contains("open-ended"));
        assert!(prompt.contains("Build a 8-week training program"));
    }

    #[test]
    fn program_prompt_honours_explicit_week_count() {
        let prompt = build_program_prompt(&program_ctx(Some(4)));
        assert!(prompt.contains("4 weeks"));
        assert!(prompt.contains("Build a 4-week training program"));
    }

    #[test]
    fn program_prompt_lists_goals_and_workouts() {
        let prompt = build_program_prompt(&program_ctx(Some(6)));
        assert!(prompt.contains("Ride a century"));
        assert!(prompt.contains("Sweet Spot 2x20"));
        assert!(prompt.contains("monday, wednesday"));
    }

    // ── build_prompt (workout suggestion) & build_fitness_prompt ──────────────────

    #[test]
    fn suggestion_prompt_includes_athlete_ftp() {
        let ctx = TrainingContext {
            athlete: AthleteProfile::default(),
            ctl: 50.0,
            atl: 45.0,
            tsb: 5.0,
            recent_sessions: Vec::new(),
            goals: Vec::new(),
            athlete_context: String::new(),
            workout_options: vec![workout("Endurance 60")],
            wellness: Vec::new(),
            time_off_dates: Vec::new(),
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("200")); // FTP watts
        assert!(prompt.contains("Endurance 60"));
    }

    #[test]
    fn fitness_prompt_describes_improving_trend() {
        let ctx = FitnessContext {
            athlete: AthleteProfile::default(),
            ctl: 60.0,
            atl: 50.0,
            tsb: 10.0,
            ctl_4wk_ago: 50.0, // +10 → improving
            week_tss: vec![300.0, 320.0, 350.0],
            total_sessions: 12,
            athlete_context: String::new(),
            wellness: Vec::new(),
        };
        let prompt = build_fitness_prompt(&ctx);
        assert!(prompt.contains("improving"));
    }
}
