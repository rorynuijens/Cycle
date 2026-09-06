use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

// ── Recent training (shared by the brief and the program builder) ────────────

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
    /// The Monday week 1 of the reply lands on. Fixed before the request so the
    /// coach is answering about real dates rather than about an unknown future.
    pub start_monday: NaiveDate,
    /// Dates the rider has already said they will not be training. A plan laid
    /// over a fortnight they are away for is a plan they will miss.
    pub time_off: Vec<NaiveDate>,
}

#[derive(Debug, Clone)]
pub struct ProgramEntry {
    pub week: u32,
    pub day: String,
    pub workout_name: String,
}

/// Planned time off, written in the coordinates the coach answers in.
///
/// The reply is `(week, weekday)` pairs, so a bare ISO date is a rule the coach
/// cannot apply — nothing in the prompt tells it which week 2026-09-20 falls
/// in, and it would have to guess. Grouping by week against the known start
/// Monday turns the rule into one it can actually follow. Dates outside the
/// weeks being planned are dropped rather than listed: a 180-day lookahead
/// against an eight-week program would otherwise spend most of its lines on
/// weeks the coach is not being asked about.
fn time_off_section(start_monday: NaiveDate, dates: &[NaiveDate], weeks: u32) -> String {
    let mut by_week: BTreeMap<i64, Vec<NaiveDate>> = BTreeMap::new();
    for date in dates {
        let offset = (*date - start_monday).num_days();
        if offset < 0 {
            continue;
        }
        let week = offset / 7 + 1;
        if week > weeks.max(1) as i64 {
            continue;
        }
        by_week.entry(week).or_default().push(*date);
    }
    if by_week.is_empty() {
        return "  None planned.".to_string();
    }
    by_week
        .into_iter()
        .map(|(week, mut days)| {
            days.sort();
            days.dedup();
            let names = days
                .iter()
                .map(|d| d.format("%A").to_string().to_lowercase())
                .collect::<Vec<_>>()
                .join(", ");
            let iso = days
                .iter()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("  - Week {week}: {names} ({iso})")
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    let time_off_text = time_off_section(ctx.start_monday, &ctx.time_off, week_count);

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
- Week 1 of your reply is the week beginning Monday {start}.

PLANNED TIME OFF — the rider is away or unavailable on these days:
{time_off}

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
- Never return a (week, day) pair listed under PLANNED TIME OFF. Move that session to another training day in the same week, or leave the week a session short — never push it into a different week.
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
        start = ctx.start_monday.format("%-d %B %Y"),
        time_off = time_off_text,
        workouts = workout_list,
        weeks = week_count,
    )
}

// ── Program revision ──────────────────────────────────────────────────────────

/// What the coach needs to replan the rest of a program the rider is part way
/// through — the plan as built, and what has actually happened to it.
pub struct ProgramRevisionContext {
    pub athlete: AthleteProfile,
    pub ctl: f64,
    pub tsb: f64,
    pub goals: Vec<AthleteGoal>,
    pub athlete_context: String,
    pub workout_options: Vec<WorkoutOption>,
    pub training_days: Vec<String>,
    /// Which week of the original plan the rider is in now.
    pub current_week: u32,
    /// Weeks still to plan, counted from the week after this one.
    pub weeks_remaining: u32,
    pub completed: usize,
    /// Sessions missed in the last fortnight, not over the whole program.
    ///
    /// The window is deliberate: a program adopted with a start date in the past
    /// carries sessions that were never rideable, and an all-time count would
    /// have the coach plan around a rider who looks like they stopped training.
    /// The prompt says "in the last fortnight" out loud for the same reason.
    pub missed: usize,
    /// The most recently missed sessions, newest first: "Wed 5 Aug — Threshold".
    pub recent_missed: Vec<String>,
    pub wellness: Vec<WellnessSnapshot>,
    /// The Monday week 1 of the reply lands on — the week after the current one.
    pub start_monday: NaiveDate,
    /// Dates the rider has already said they will not be training.
    pub time_off: Vec<NaiveDate>,
}

/// Ask the coach to replan the remainder of a program.
///
/// Deliberately separate from [`build_program_prompt`] rather than a flag on
/// it: this prompt has to argue against the plan it is replacing, and the two
/// sets of instructions pull in different directions.
pub fn build_program_revision_prompt(ctx: &ProgramRevisionContext) -> String {
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

    let missed_text = if ctx.recent_missed.is_empty() {
        "  None.".to_string()
    } else {
        ctx.recent_missed
            .iter()
            .map(|m| format!("  - {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let wellness_text = if ctx.wellness.is_empty() {
        "  No recent readings.".to_string()
    } else {
        ctx.wellness
            .iter()
            .map(|w| {
                let rhr = w
                    .resting_hr
                    .map(|v| format!("resting HR {v}"))
                    .unwrap_or_else(|| "resting HR —".into());
                let sleep = w
                    .sleep_score
                    .map(|v| format!("sleep score {v}"))
                    .unwrap_or_else(|| "sleep score —".into());
                format!("  - {}: {rhr}, {sleep}", w.date)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let time_off_text =
        time_off_section(ctx.start_monday, &ctx.time_off, ctx.weeks_remaining.max(1));

    format!(
        r#"You are an expert cycling coach revising a training program already under way.

{context_section}ATHLETE:
- FTP: {ftp} W · {weight:.1} kg
- Current CTL (fitness): {ctl:.0}
- Current TSB (form): {tsb:+.0}

GOALS:
{goals}

THE PROGRAM SO FAR:
- The rider is in week {current_week}.
- Sessions completed: {completed}
- Sessions missed in the last fortnight: {missed}

RECENTLY MISSED SESSIONS:
{missed_list}

RECENT WELLNESS:
{wellness}

PLANNED TIME OFF — the rider is away or unavailable on these days:
{time_off}

TRAINING SCHEDULE:
- Training days: {days}

AVAILABLE WORKOUTS:
{workouts}

Replan the next {weeks} weeks, starting from next week. Week 1 of your reply is the week beginning Monday {start}.

Take the rider's actual training into account rather than the plan they were given:
- Missed sessions are gone. Do NOT try to make up lost work by adding volume or intensity.
- If form (TSB) is very negative, or wellness is trending badly, start easier and rebuild.
- If the rider has been consistent and form is good, progress normally.
- Keep applying progressive overload with a lighter recovery week every fourth week.

Return ONLY a JSON array — no text before or after it — in exactly this format:
[
  {{"week": 1, "day": "monday", "workout": "Exact Workout Name"}},
  {{"week": 1, "day": "wednesday", "workout": "Exact Workout Name"}}
]

Rules:
- Use only workout names that appear exactly in the AVAILABLE WORKOUTS list above.
- Use only the days listed in TRAINING SCHEDULE.
- Never return a (week, day) pair listed under PLANNED TIME OFF. Move that session to another training day in the same week, or leave the week a session short — never push it into a different week.
- Day values must be lowercase full day names: monday, tuesday, wednesday, thursday, friday, saturday, sunday."#,
        context_section = context_section,
        ftp = ctx.athlete.ftp_watts,
        weight = ctx.athlete.weight_kg,
        ctl = ctx.ctl,
        tsb = ctx.tsb,
        goals = goals_text,
        current_week = ctx.current_week,
        completed = ctx.completed,
        missed = ctx.missed,
        missed_list = missed_text,
        wellness = wellness_text,
        time_off = time_off_text,
        start = ctx.start_monday.format("%-d %B %Y"),
        days = ctx.training_days.join(", "),
        workouts = workout_list,
        weeks = ctx.weeks_remaining.max(1),
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
    /// Sent explicitly rather than left out. Sonnet 5 runs adaptive thinking
    /// when this field is absent — where Sonnet 4.6 ran without it — and
    /// `max_tokens` caps thinking and answer *together*. A coach reply budgeted
    /// at 1024 tokens could then be spent reasoning, and the rider would get a
    /// truncated answer, or none at all.
    thinking: Thinking,
    messages: Vec<ClaudeMessage<'a>>,
}

/// The request's thinking setting. Only the disabled form is constructed here;
/// turning thinking on means raising every caller's `max_tokens` to match.
#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl Thinking {
    fn disabled() -> Self {
        Self { kind: "disabled" }
    }
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

/// Sonnet 5 — the like-for-like successor to the Sonnet 4.6 this app was built
/// against, keeping the Sonnet price tier the rider pays for on their own key.
const CLAUDE_MODEL: &str = "claude-sonnet-5";

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
        thinking: Thinking::disabled(),
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

    // ── Request shape ────────────────────────────────────────────────────────

    fn request_json() -> serde_json::Value {
        serde_json::to_value(ClaudeRequest {
            model: CLAUDE_MODEL,
            max_tokens: 1024,
            thinking: Thinking::disabled(),
            messages: vec![ClaudeMessage {
                role: "user",
                content: "hello",
            }],
        })
        .expect("request serialises")
    }

    #[test]
    fn should_send_thinking_disabled_so_the_token_budget_is_all_answer() {
        // Left out, Sonnet 5 thinks by default and spends the same max_tokens
        // doing it — which is how a coach reply comes back truncated or empty.
        assert_eq!(request_json()["thinking"]["type"], "disabled");
    }

    #[test]
    fn should_request_a_current_model() {
        // Guards the swap away from Sonnet 4.6; `-4-` catches a slip back to
        // any 4.x id without pinning this test to one specific model name.
        assert!(
            !CLAUDE_MODEL.contains("-4-"),
            "CLAUDE_MODEL is still on a 4.x model: {CLAUDE_MODEL}"
        );
    }

    #[test]
    fn should_send_the_prompt_as_a_single_user_message() {
        let json = request_json();
        assert_eq!(json["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "hello");
    }

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

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    /// Monday 7 September 2026 — week 1 of every program built in these tests.
    fn start() -> NaiveDate {
        date(2026, 9, 7)
    }

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
            start_monday: start(),
            time_off: Vec::new(),
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

    // ── time_off_section ─────────────────────────────────────────────────────

    #[test]
    fn should_say_none_planned_when_the_rider_is_never_away() {
        assert_eq!(time_off_section(start(), &[], 8), "  None planned.");
    }

    #[test]
    fn should_place_a_day_off_in_the_week_the_coach_will_number_it() {
        // Thursday 17 Sep is 10 days after Monday 7 Sep, so week 2.
        let text = time_off_section(start(), &[date(2026, 9, 17)], 8);
        assert_eq!(text, "  - Week 2: thursday (2026-09-17)");
    }

    #[test]
    fn should_group_a_holiday_by_week_rather_than_listing_bare_dates() {
        let away: Vec<NaiveDate> = (14..=20).map(|d| date(2026, 9, d)).collect();
        let text = time_off_section(start(), &away, 8);
        // The whole of week 2, named the way the reply names days.
        assert!(text.contains(
            "- Week 2: monday, tuesday, wednesday, thursday, friday, \
                               saturday, sunday"
        ));
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn should_split_a_holiday_that_straddles_two_weeks() {
        let away = vec![date(2026, 9, 12), date(2026, 9, 13), date(2026, 9, 14)];
        let text = time_off_section(start(), &away, 8);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  - Week 1: saturday, sunday"));
        assert!(lines[1].starts_with("  - Week 2: monday"));
    }

    #[test]
    fn should_drop_time_off_before_the_program_starts() {
        // Already behind the rider — the plan is not being laid over it.
        assert_eq!(
            time_off_section(start(), &[date(2026, 9, 6)], 8),
            "  None planned."
        );
    }

    #[test]
    fn should_drop_time_off_beyond_the_weeks_being_planned() {
        // The lookahead runs 180 days; a 4-week program is not being asked
        // about week 12, and listing it would bury the weeks that matter.
        let text = time_off_section(start(), &[date(2026, 11, 26)], 4);
        assert_eq!(text, "  None planned.");
    }

    #[test]
    fn should_keep_time_off_in_the_final_week_of_the_program() {
        // Boundary: week 4 of a 4-week plan is still being planned.
        // Mon 28 Sep is 21 days after the start — the first day of week 4.
        let text = time_off_section(start(), &[date(2026, 9, 28)], 4);
        assert_eq!(text, "  - Week 4: monday (2026-09-28)");
    }

    #[test]
    fn program_prompt_pins_the_week_the_coach_is_planning_from() {
        let prompt = build_program_prompt(&program_ctx(Some(4)));
        assert!(
            prompt.contains("Week 1 of your reply is the week beginning Monday 7 September 2026")
        );
    }

    #[test]
    fn program_prompt_states_time_off_in_week_and_day_not_iso_dates() {
        let mut ctx = program_ctx(Some(4));
        ctx.time_off = vec![date(2026, 9, 16)];
        let prompt = build_program_prompt(&ctx);
        assert!(prompt.contains("Week 2: wednesday"));
        assert!(prompt.contains("Never return a (week, day) pair listed under PLANNED TIME OFF"));
    }
}
