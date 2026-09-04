//! The one interpretation of a rider's day.
//!
//! The app used to ask the coach three separate questions — how am I this
//! morning, what does my fitness look like, what should I ride — from three
//! separate cards, each billed separately and each cached forever. Asked hours
//! apart from data that had moved in between, they answered differently, and a
//! rider reading two of them was reading two coaches.
//!
//! This module asks once. One request, written from one read of the database,
//! parsed into the slices each card shows. The sections are separate so the
//! cards can differ in what they show; the *reply* is single so they cannot
//! differ in what they say.
//!
//! What it deliberately does not do is pick the session. That belongs to
//! [`crate::training::program`], which is the only thing here with a memory
//! longer than one morning — see [`CoachVerdict`].

pub mod input;
pub mod parse;
pub mod prompt;

// The verdict is a plain training concept, defined where the rules that act on
// it live (CLAUDE.md §2.6). Re-exported so callers can speak of it as part of
// the brief, which is where it reaches them.
pub use crate::training::program::CoachVerdict;

/// Bumped whenever the sections or the parse change shape.
///
/// A brief stamped with any other version is ignored rather than half
/// understood: the cost is one regenerated brief, and the alternative is a card
/// rendering a section that no longer means what it did.
pub const BRIEF_VERSION: u32 = 1;

/// One morning's interpretation, split into the slices each card shows.
///
/// Every section is optional on purpose. A reply that arrives truncated or
/// mangled should cost the rider one card, not all of them — so the sections
/// that did arrive are kept and the rest come back as `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DailyBrief {
    /// See [`BRIEF_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// ISO `YYYY-MM-DD` this was written for. A brief from yesterday is worse
    /// than none: it opens with plans the rider has already ridden.
    #[serde(default)]
    pub written_for: String,
    /// The inputs it was written from, so a card can tell the rider it has been
    /// overtaken without spending anything to find out.
    #[serde(default)]
    pub fingerprint: String,

    /// How the rider is this morning — form and wellness read together.
    #[serde(default)]
    pub readiness: Option<String>,
    /// What the training load has been doing. The Fitness page's slice.
    #[serde(default)]
    pub form: Option<String>,
    /// What to do about today, and why. The Coaching page's slice.
    #[serde(default)]
    pub session: Option<String>,
    /// Pre, intra and post-ride fuelling.
    #[serde(default)]
    pub fueling: Option<String>,
    /// The whole cleaned reply, kept only when it carried no section markers at
    /// all. Better a wall of text on the dashboard than a blank card.
    #[serde(default)]
    pub unstructured: Option<String>,

    /// How hard today should be. Never which session — see [`CoachVerdict`].
    #[serde(default)]
    pub verdict: CoachVerdict,
    /// The session the brief was written about, whoever put it there.
    #[serde(default)]
    pub planned_workout: Option<String>,
    /// Whether a training program owned the day.
    ///
    /// Carried on the brief because the cards need it after a restart, and
    /// because it decides which actions they may offer: swapping or deleting a
    /// programmed session behind the program's back drops work the plan was
    /// counting on. With a program running, easing belongs to the Coaching
    /// page, where the plan can record what it did.
    #[serde(default)]
    pub program_active: bool,
    /// A workout the brief chose itself. Only ever `Some` when no program owned
    /// the day — [`parse_brief`] drops it otherwise.
    #[serde(default)]
    pub recommended_workout: Option<String>,
}

impl DailyBrief {
    /// Whether this was written for `today`.
    pub fn is_for(&self, today: &str) -> bool {
        !self.written_for.is_empty() && self.written_for == today
    }

    /// Whether the inputs have moved since this was written.
    ///
    /// A brief carrying no fingerprint reads as current. It was written before
    /// fingerprinting existed, or by a build that could not compute one, and
    /// neither is a reason to tell the rider their morning is out of date.
    pub fn is_stale_for(&self, current: &str) -> bool {
        !self.fingerprint.is_empty() && self.fingerprint != current
    }

    /// Nothing came back that any card could show.
    pub fn is_empty(&self) -> bool {
        self.readiness.is_none()
            && self.form.is_none()
            && self.session.is_none()
            && self.fueling.is_none()
            && self.unstructured.is_none()
    }

    /// The dashboard's slice: the whole brief, headed, in reading order.
    ///
    /// Headings are added here rather than asked for in the prompt so the other
    /// two cards can show their section without one.
    pub fn full_prose(&self) -> String {
        if let Some(raw) = &self.unstructured {
            return raw.clone();
        }
        let mut out: Vec<String> = Vec::new();
        for (heading, body) in [
            ("Readiness", &self.readiness),
            ("Your form", &self.form),
            ("Today", &self.session),
            ("Fuelling", &self.fueling),
        ] {
            if let Some(text) = body.as_deref().filter(|t| !t.trim().is_empty()) {
                out.push(format!("**{heading}**\n\n{text}"));
            }
        }
        out.join("\n\n")
    }

    /// The Fitness page's slice, falling back to the readiness read.
    ///
    /// The fallback matters more than it looks: with the section missing, the
    /// alternative is an empty card on a page the rider opened to see exactly
    /// this, and readiness is drawn from the same numbers.
    pub fn form_slice(&self) -> Option<&str> {
        present(&self.form).or_else(|| present(&self.readiness))
    }

    /// The Coaching page's slice, falling back to the readiness read.
    pub fn session_slice(&self) -> Option<&str> {
        present(&self.session).or_else(|| present(&self.readiness))
    }
}

/// A section that is present and not just whitespace.
fn present(section: &Option<String>) -> Option<&str> {
    section.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

// ── Generating one ────────────────────────────────────────────────────────────

/// The answer's token budget.
///
/// Four sections at roughly 450 words. It replaces the 1600, 1400 and 1400 the
/// three separate calls used to spend between them, and the input side saves
/// far more than that: one prompt instead of three that overlapped almost
/// entirely. The headroom over the expected answer is deliberate — truncation
/// is the one failure that bills the rider in full for something they cannot
/// read, so it is the cheapest thing here to over-provide.
const MAX_TOKENS: u32 = 2400;

/// Why a brief could not be written.
///
/// Mirrors the shape the UI already shows for a failed request; kept here so
/// [`generate`] stays free of UI types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BriefError {
    /// The training history could not be read, so nothing was sent.
    DataUnavailable,
    /// The request went out and did not come back with an answer.
    Request,
}

/// Write today's brief, and cache it.
///
/// Runs entirely on the tokio side — it touches the network and the database,
/// and never a widget (CLAUDE.md §2.3).
///
/// Saving happens here rather than in the caller so that a brief which cost the
/// rider money is on disk before anything can go wrong with displaying it.
pub async fn generate(
    pool: &sqlx::SqlitePool,
    api_key: &str,
    athlete: crate::data::athlete::AthleteProfile,
    today: chrono::NaiveDate,
) -> Result<DailyBrief, BriefError> {
    let today_str = today.format("%Y-%m-%d").to_string();

    let data = input::load_brief_input(pool, today).await.map_err(|e| {
        tracing::error!("Could not read the training history to brief on: {e}");
        BriefError::DataUnavailable
    })?;

    let metrics = crate::training::fitness::compute_load_metrics(
        &data.records,
        &data.intervals_pairs,
        athlete.ftp_watts,
        today,
    );
    let fingerprint = input::BriefInputs::of(&data, &athlete, today).fingerprint();
    let plan = data.today_plan(today);
    let program_active = plan.program_active();
    let planned_name = plan.planned().map(|w| w.name.clone());

    let ctx = build_context(&data, &athlete, &metrics, plan, today, &today_str);
    let prompt = prompt::build_brief_prompt(&ctx);

    let reply = crate::ai::coach::get_suggestion(api_key, &prompt, MAX_TOKENS)
        .await
        .map_err(|e| {
            tracing::error!("The morning brief request failed: {e}");
            BriefError::Request
        })?;

    let brief = parse::parse_brief(
        &reply,
        &today_str,
        &fingerprint,
        program_active,
        planned_name.as_deref(),
    );

    if let Err(e) = crate::data::ai_cache::save_daily_brief(pool, &brief).await {
        // The rider has already paid for this. Show it even if it will be gone
        // by the next launch.
        tracing::error!("Could not cache the brief that was just generated: {e}");
    }
    Ok(brief)
}

/// Assemble the prompt context from one read of the database.
fn build_context(
    data: &input::BriefInput,
    athlete: &crate::data::athlete::AthleteProfile,
    metrics: &crate::training::fitness::LoadMetrics,
    plan: prompt::TodayPlan,
    today: chrono::NaiveDate,
    today_str: &str,
) -> prompt::BriefContext {
    use crate::ai::context::{
        build_recent_session, icu_activity_to_recent_session, wellness_snapshots,
        workouts_as_options,
    };

    // Both sources of training, newest first, so the coach reads one history
    // rather than two.
    let mut recent_sessions: Vec<_> = data
        .records
        .iter()
        .map(|r| build_recent_session(r, athlete.ftp_watts))
        .chain(
            data.icu_activities
                .iter()
                .map(icu_activity_to_recent_session),
        )
        .collect();
    recent_sessions.sort_by(|a, b| b.date.cmp(&a.date));
    recent_sessions.truncate(MAX_RECENT_SESSIONS);

    prompt::BriefContext {
        athlete: athlete.clone(),
        ctl: metrics.ctl,
        atl: metrics.atl,
        tsb: metrics.tsb(),
        ctl_4wk_ago: metrics.ctl_4wk_ago,
        week_tss: crate::training::analytics::compute_weekly_tss_from_summaries(
            &data.records,
            &data.intervals_pairs,
            athlete.ftp_watts,
            today,
            input::TSS_WEEKS,
        )
        .into_iter()
        .map(|(_, tss)| tss)
        .collect(),
        total_sessions: data.records.len() + data.icu_count as usize,
        recent_sessions,
        wellness: wellness_snapshots(&data.wellness),
        // Computed here rather than in the prompt: the arithmetic is a
        // training concept and `ai` only formats it (CLAUDE.md §2.6).
        //
        // Measured on the newest reading that exists rather than on `today`.
        // Insisting on today's would blank this block on exactly the mornings
        // it matters most — the overnight sync has not run, the numbers are two
        // days old, and a coach reading raw rows is precisely then most likely
        // to miss the outlier. The heading above dates the reading, so nothing
        // is passed off as this morning's.
        wellness_readings: data
            .wellness
            .iter()
            .map(|w| w.date)
            .max()
            .map(|latest| {
                crate::training::analytics::wellness_readings(
                    &data.wellness,
                    latest,
                    crate::training::analytics::MIN_WELLNESS_READINGS,
                )
            })
            .unwrap_or_default(),
        goals: data.goals.clone(),
        athlete_context: data.athlete_context.clone(),
        // Only offered when the brief may choose, but built either way: the
        // prompt decides whether to print it.
        workout_options: workouts_as_options(&data.workouts, &data.icu_workouts),
        plan,
        time_off_dates: data
            .time_off
            .iter()
            .map(|t| t.date.format("%Y-%m-%d").to_string())
            .collect(),
        today: today_str.to_string(),
    }
}

/// How many recent sessions the coach is shown.
///
/// Enough to see the shape of the last few weeks without spending the budget on
/// history the form numbers already summarise.
const MAX_RECENT_SESSIONS: usize = 12;

// ── Deciding whether to ask ───────────────────────────────────────────────────

/// What start-up should do about the brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// Show what is cached; it still applies.
    Show,
    /// Show what is cached, and tell the rider it has been overtaken.
    ShowOutOfDate,
    /// Ask for a new one. This is the only variant that spends money.
    Generate,
    /// There is nothing cached and no key to fetch one with.
    NeedKey,
}

/// Decide what to do at start-up.
///
/// Pure, and deliberately not inlined into the store: this is the function that
/// decides whether the rider is billed, and it should be readable and testable
/// on its own rather than buried in a widget callback.
///
/// `cached` is `None` both when nothing is stored and when what is stored could
/// not be read — see [`crate::data::ai_cache::daily_brief`]. Both mean "there
/// is nothing to show", and both are a reason to generate.
///
/// A read that *failed* is a different matter and never reaches here: the
/// caller stops first, because a database hiccup must not bill anyone.
pub fn startup_action(
    cached: Option<&DailyBrief>,
    today: &str,
    fingerprint: &str,
    has_key: bool,
) -> StartupAction {
    match cached {
        Some(brief) if brief.is_for(today) && !brief.is_empty() => {
            if brief.is_stale_for(fingerprint) {
                // Overtaken, but still the rider's own morning. Show it and let
                // them decide whether it is worth replacing.
                StartupAction::ShowOutOfDate
            } else {
                StartupAction::Show
            }
        }
        _ if has_key => StartupAction::Generate,
        _ => StartupAction::NeedKey,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> DailyBrief {
        DailyBrief {
            version: BRIEF_VERSION,
            written_for: "2026-08-11".into(),
            fingerprint: "v1|rides:10".into(),
            readiness: Some("You slept well.".into()),
            form: Some("Fitness is building.".into()),
            session: Some("Ride the threshold session.".into()),
            fueling: Some("Pilav before, ayran after.".into()),
            ..Default::default()
        }
    }

    // ── Dates and fingerprints ───────────────────────────────────────────────

    #[test]
    fn should_recognise_yesterdays_brief_as_written_for_another_day() {
        assert!(brief().is_for("2026-08-11"));
        assert!(!brief().is_for("2026-08-12"));
    }

    #[test]
    fn should_treat_an_undated_brief_as_written_for_no_day() {
        let b = DailyBrief {
            written_for: String::new(),
            ..brief()
        };
        assert!(
            !b.is_for("2026-08-11"),
            "and not for the empty string either"
        );
        assert!(!b.is_for(""));
    }

    #[test]
    fn should_be_stale_when_the_inputs_have_moved() {
        assert!(brief().is_stale_for("v1|rides:11"));
        assert!(!brief().is_stale_for("v1|rides:10"));
    }

    #[test]
    fn should_not_be_stale_when_it_carries_no_fingerprint() {
        // Never tell a rider their morning is out of date because this build
        // could not work out whether it was.
        let b = DailyBrief {
            fingerprint: String::new(),
            ..brief()
        };
        assert!(!b.is_stale_for("v1|rides:11"));
    }

    // ── Slices ───────────────────────────────────────────────────────────────

    #[test]
    fn should_give_each_card_its_own_section() {
        let b = brief();
        assert_eq!(b.form_slice(), Some("Fitness is building."));
        assert_eq!(b.session_slice(), Some("Ride the threshold session."));
    }

    #[test]
    fn should_fall_back_to_readiness_when_a_section_is_missing() {
        let b = DailyBrief {
            form: None,
            session: None,
            ..brief()
        };
        assert_eq!(b.form_slice(), Some("You slept well."));
        assert_eq!(b.session_slice(), Some("You slept well."));
    }

    #[test]
    fn should_treat_a_blank_section_as_missing() {
        let b = DailyBrief {
            form: Some("   \n ".into()),
            ..brief()
        };
        assert_eq!(b.form_slice(), Some("You slept well."));
    }

    #[test]
    fn should_have_nothing_to_show_when_every_section_is_absent() {
        assert!(DailyBrief::default().is_empty());
        assert_eq!(DailyBrief::default().form_slice(), None);
        assert!(!brief().is_empty());
    }

    // ── Full prose ───────────────────────────────────────────────────────────

    #[test]
    fn should_head_each_section_in_reading_order() {
        let out = brief().full_prose();
        let headings: Vec<&str> = out.lines().filter(|l| l.starts_with("**")).collect();
        assert_eq!(
            headings,
            [
                "**Readiness**",
                "**Your form**",
                "**Today**",
                "**Fuelling**"
            ]
        );
        assert!(out.contains("Pilav before, ayran after."));
    }

    #[test]
    fn should_skip_a_missing_section_rather_than_heading_an_empty_one() {
        let b = DailyBrief {
            fueling: None,
            form: Some(String::new()),
            ..brief()
        };
        let out = b.full_prose();
        assert!(!out.contains("Fuelling"));
        assert!(!out.contains("Your form"));
        assert!(out.contains("Readiness"));
    }

    #[test]
    fn should_show_an_unstructured_reply_verbatim() {
        // No markers came back, so there is nothing to head — but the coach did
        // answer, and the rider should see it.
        let b = DailyBrief {
            unstructured: Some("Take the day off.".into()),
            ..DailyBrief::default()
        };
        assert_eq!(b.full_prose(), "Take the day off.");
        assert!(!b.is_empty());
    }

    // ── Round trip ───────────────────────────────────────────────────────────

    #[test]
    fn should_round_trip_through_json() {
        let b = DailyBrief {
            verdict: CoachVerdict::Ease,
            planned_workout: Some("Threshold 3x12".into()),
            ..brief()
        };
        let json = serde_json::to_string(&b).expect("a brief serialises");
        assert_eq!(
            serde_json::from_str::<DailyBrief>(&json).expect("and reads back"),
            b
        );
    }

    // ── What start-up does ───────────────────────────────────────────────────

    const TODAY: &str = "2026-08-11";
    const FP: &str = "v1|rides:10";

    #[test]
    fn should_show_without_generating_when_the_cached_brief_is_current() {
        assert_eq!(
            startup_action(Some(&brief()), TODAY, FP, true),
            StartupAction::Show
        );
    }

    #[test]
    fn should_show_out_of_date_without_generating_when_the_inputs_moved() {
        // The rule the whole refresh policy rests on: a ride recorded since
        // this morning marks the card, it does not spend the rider's money.
        assert_eq!(
            startup_action(Some(&brief()), TODAY, "v1|rides:11", true),
            StartupAction::ShowOutOfDate
        );
    }

    #[test]
    fn should_generate_when_nothing_is_cached() {
        assert_eq!(
            startup_action(None, TODAY, FP, true),
            StartupAction::Generate
        );
    }

    #[test]
    fn should_generate_when_the_cached_brief_is_for_yesterday() {
        // However fresh its inputs still look, it opens with plans the rider
        // has already ridden.
        assert_eq!(
            startup_action(Some(&brief()), "2026-08-12", FP, true),
            StartupAction::Generate
        );
    }

    #[test]
    fn should_generate_when_todays_brief_came_back_empty() {
        // A request that produced nothing showable should not count as done.
        let empty = DailyBrief {
            readiness: None,
            form: None,
            session: None,
            fueling: None,
            ..brief()
        };
        assert_eq!(
            startup_action(Some(&empty), TODAY, FP, true),
            StartupAction::Generate
        );
    }

    #[test]
    fn should_ask_for_a_key_rather_than_generating_when_none_is_stored() {
        assert_eq!(
            startup_action(None, TODAY, FP, false),
            StartupAction::NeedKey
        );
    }

    #[test]
    fn should_still_show_a_cached_brief_after_the_key_is_removed() {
        // The brief was already paid for. Losing the key should not blank it.
        assert_eq!(
            startup_action(Some(&brief()), TODAY, FP, false),
            StartupAction::Show
        );
    }

    #[test]
    fn should_not_generate_from_an_unfingerprinted_brief() {
        // Written before fingerprinting existed. That is not a reason to bill.
        let b = DailyBrief {
            fingerprint: String::new(),
            ..brief()
        };
        assert_eq!(
            startup_action(Some(&b), TODAY, FP, true),
            StartupAction::Show
        );
    }

    #[test]
    fn should_read_a_brief_written_before_a_field_existed() {
        // Every field is #[serde(default)] so a stored brief from an older
        // build still parses; the version check decides whether to use it.
        let b: DailyBrief =
            serde_json::from_str(r#"{"written_for":"2026-08-11"}"#).expect("a sparse brief reads");
        assert_eq!(b.written_for, "2026-08-11");
        assert_eq!(b.verdict, CoachVerdict::Proceed);
        assert!(b.is_empty());
    }
}
