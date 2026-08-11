//! Reading a reply back into the slices the cards show.
//!
//! This never fails. A brief is one request standing behind three cards, so an
//! answer that arrives mangled must cost the rider whichever section was
//! mangled and nothing else — blanking the dashboard, the Fitness page and the
//! Coaching page together because one marker went missing would be a worse
//! outcome than the problem it reports.
//!
//! The ladder it climbs down, rung by rung:
//!
//! 1. Every marker present — each card gets its own section.
//! 2. Some missing — what arrived renders, the rest fall back (see
//!    [`DailyBrief::form_slice`]).
//! 3. None at all — the whole reply becomes `unstructured` and the dashboard
//!    shows it verbatim.
//! 4. No verdict line — scan for a keyword, then default to proceeding, which
//!    is the verdict that changes nothing.

use super::prompt::{
    MARKER_RECOMMENDED, MARKER_VERDICT, SECTION_FORM, SECTION_FUELING, SECTION_READINESS,
    SECTION_SESSION,
};
use super::{DailyBrief, BRIEF_VERSION};
use crate::ai::naming::{clean_workout_name, extract_marker_value, names_match};
use crate::training::program::CoachVerdict;

/// Read a reply into a brief.
///
/// `program_active` is enforcement rather than decoration. The prompt asks for
/// no workout name when a program owns the day, but a prompt is a request and
/// not a guarantee; dropping the name here means the precedence rule holds even
/// on the reply that ignored it.
pub fn parse_brief(
    text: &str,
    today: &str,
    fingerprint: &str,
    program_active: bool,
    planned_name: Option<&str>,
) -> DailyBrief {
    let mut brief = DailyBrief {
        version: BRIEF_VERSION,
        written_for: today.to_string(),
        fingerprint: fingerprint.to_string(),
        verdict: parse_verdict(text),
        planned_workout: planned_name.map(str::to_string),
        program_active,
        ..DailyBrief::default()
    };

    let sections = split_sections(text);
    if sections.is_empty() {
        // Nothing carried a marker. The coach still answered, so show what it
        // said rather than an empty card.
        brief.unstructured = non_empty(clean_body(text));
    } else {
        for (marker, body) in sections {
            let body = non_empty(clean_body(&body));
            match marker.as_str() {
                SECTION_READINESS => brief.readiness = body,
                SECTION_FORM => brief.form = body,
                SECTION_SESSION => brief.session = body,
                SECTION_FUELING => brief.fueling = body,
                other => {
                    // A marker we never asked for. Dropping the body is right —
                    // it belongs to no card — but the sections after it must
                    // still parse, which is why this is not an early return.
                    tracing::debug!("brief carried an unknown section marker: {other}");
                }
            }
        }
    }

    brief.recommended_workout = parse_recommendation(text, program_active, planned_name);
    brief
}

/// Split the reply on its section markers, in the order they appear.
///
/// Anything before the first marker is dropped: it is preamble the prompt did
/// not ask for, and it belongs to no card.
fn split_sections(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;

    for line in text.lines() {
        if let Some(marker) = section_marker(line) {
            if let Some((name, body)) = current.take() {
                sections.push((name, body.join("\n")));
            }
            current = Some((marker, Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((name, body)) = current {
        sections.push((name, body.join("\n")));
    }
    sections
}

/// The marker a line is, if it is one.
///
/// Matched on the whole trimmed line so a marker quoted inside a sentence does
/// not silently start a new section. Markdown decoration is tolerated because
/// models add it unbidden: `**===FORM===**` is still a form marker.
fn section_marker(line: &str) -> Option<String> {
    let trimmed = line.trim().trim_matches('*').trim();
    let looks_like_marker = trimmed.starts_with("===") && trimmed.ends_with("===");
    (looks_like_marker && trimmed.len() > 6).then(|| trimmed.to_uppercase())
}

/// Strip what the rider should never see from a section body.
fn clean_body(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let upper = line.trim().trim_matches('*').trim().to_uppercase();
            // The trailer lines are instructions to the parser, not prose.
            !upper.starts_with(MARKER_VERDICT) && !upper.starts_with(MARKER_RECOMMENDED)
        })
        // Requests go out with thinking disabled, but a leaked tag renders as
        // literal markup in the card, so drop them rather than trust that.
        .filter(|line| !is_bare_tag(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// A line that is nothing but an XML-ish tag, e.g. a leaked `<thinking>`.
///
/// Only whole-line tags are dropped: a `<` in the middle of a sentence is the
/// rider's prose, and Pango will render it fine.
fn is_bare_tag(line: &str) -> bool {
    let t = line.trim();
    t.len() > 2 && t.starts_with('<') && t.ends_with('>') && !t.contains(' ')
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// The verdict on the trailer line, or the best guess without one.
fn parse_verdict(text: &str) -> CoachVerdict {
    if let Some(value) = extract_marker_value(text, MARKER_VERDICT) {
        // Decoration first: models write `**VERDICT:** EASE` unprompted.
        let value = value.trim().trim_matches('*').trim().to_uppercase();
        if value.starts_with("REST")
            || value.starts_with("TIME_OFF")
            || value.starts_with("TIME OFF")
        {
            return CoachVerdict::Rest;
        }
        if value.starts_with("EASE") {
            return CoachVerdict::Ease;
        }
        if value.starts_with("PROCEED") {
            return CoachVerdict::Proceed;
        }
    }

    // No usable marker. Scan for the words, then assume the plan stands —
    // guessing "rest" from an unreadable reply would cancel training the rider
    // was ready for.
    let upper = text.to_uppercase();
    if upper.contains("REST DAY") || upper.contains("TIME OFF") {
        CoachVerdict::Rest
    } else if upper.contains("EASE") || upper.contains("LIGHTER") {
        CoachVerdict::Ease
    } else {
        CoachVerdict::Proceed
    }
}

/// The workout the reply named, when it was entitled to name one.
fn parse_recommendation(
    text: &str,
    program_active: bool,
    planned_name: Option<&str>,
) -> Option<String> {
    if program_active {
        // The program owns the session. A name here is the model overstepping
        // the prompt, and acting on it would drop a session from a progression
        // that never makes it up.
        return None;
    }

    let name = clean_workout_name(&extract_marker_value(text, MARKER_RECOMMENDED)?);
    if name.is_empty() {
        return None;
    }
    // Naming the session already on the calendar is agreement, not a swap.
    // Offering a Replace button that swaps a workout for itself is nonsense.
    if planned_name.is_some_and(|planned| names_match(&name, planned)) {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed reply, as the prompt asks for it.
    fn full_reply() -> String {
        format!(
            "{SECTION_READINESS}\nYou slept well and your form is fresh.\n\n\
             {SECTION_FORM}\nFitness is building steadily.\n\n\
             {SECTION_SESSION}\nRide the threshold session as written.\n\n\
             {SECTION_FUELING}\nPilav two hours before, ayran after.\n\n\
             {MARKER_VERDICT} PROCEED\n"
        )
    }

    fn parse(text: &str) -> DailyBrief {
        parse_brief(text, "2026-08-11", "fp1", false, None)
    }

    // ── The happy path ───────────────────────────────────────────────────────

    #[test]
    fn should_split_every_section_when_all_markers_are_present() {
        let b = parse(&full_reply());
        assert_eq!(
            b.readiness.as_deref(),
            Some("You slept well and your form is fresh.")
        );
        assert_eq!(b.form.as_deref(), Some("Fitness is building steadily."));
        assert_eq!(
            b.session.as_deref(),
            Some("Ride the threshold session as written.")
        );
        assert_eq!(
            b.fueling.as_deref(),
            Some("Pilav two hours before, ayran after.")
        );
        assert_eq!(b.verdict, CoachVerdict::Proceed);
        assert!(
            b.unstructured.is_none(),
            "a structured reply needs no fallback"
        );
    }

    #[test]
    fn should_stamp_the_brief_with_the_day_and_inputs_it_was_written_from() {
        let b = parse_brief(&full_reply(), "2026-08-11", "fp-abc", false, None);
        assert_eq!(b.version, BRIEF_VERSION);
        assert!(b.is_for("2026-08-11"));
        assert_eq!(b.fingerprint, "fp-abc");
    }

    #[test]
    fn should_keep_the_marker_lines_out_of_the_prose() {
        let b = parse(&full_reply());
        let prose = b.full_prose();
        assert!(!prose.contains("VERDICT"));
        assert!(!prose.contains("==="));
    }

    // ── Degrading ────────────────────────────────────────────────────────────

    #[test]
    fn should_keep_the_sections_that_survive_when_one_marker_is_missing() {
        let reply = format!(
            "{SECTION_READINESS}\nFresh today.\n{SECTION_SESSION}\nRide it.\n{MARKER_VERDICT} PROCEED"
        );
        let b = parse(&reply);
        assert_eq!(b.readiness.as_deref(), Some("Fresh today."));
        assert_eq!(b.session.as_deref(), Some("Ride it."));
        assert_eq!(b.form, None);
        // The Fitness card still has something to show.
        assert_eq!(b.form_slice(), Some("Fresh today."));
    }

    #[test]
    fn should_return_the_whole_reply_as_unstructured_when_no_marker_is_found() {
        let b = parse("You are fresh. Ride the threshold session.\nVERDICT: PROCEED");
        assert_eq!(
            b.unstructured.as_deref(),
            Some("You are fresh. Ride the threshold session."),
            "the trailer is still stripped"
        );
        assert!(!b.is_empty(), "the dashboard must not be blank");
        assert_eq!(b.verdict, CoachVerdict::Proceed);
    }

    #[test]
    fn should_ignore_an_unknown_marker_without_losing_the_section_after_it() {
        let reply = format!(
            "===MOOD===\nCheerful.\n{SECTION_FORM}\nBuilding well.\n{MARKER_VERDICT} PROCEED"
        );
        let b = parse(&reply);
        assert_eq!(b.form.as_deref(), Some("Building well."));
        assert!(!b.full_prose().contains("Cheerful"));
    }

    #[test]
    fn should_not_panic_on_a_reply_truncated_mid_section() {
        // What the token budget running out actually looks like: the early
        // sections arrived, the last one stops mid-word, and there is no
        // trailer at all.
        let reply = format!(
            "{SECTION_READINESS}\nFresh.\n{SECTION_FORM}\nBuilding.\n{SECTION_SESSION}\nRide the thre"
        );
        let b = parse(&reply);
        assert_eq!(b.readiness.as_deref(), Some("Fresh."));
        assert_eq!(b.form.as_deref(), Some("Building."));
        assert_eq!(b.session.as_deref(), Some("Ride the thre"));
        assert_eq!(
            b.verdict,
            CoachVerdict::Proceed,
            "no trailer means no change"
        );
    }

    #[test]
    fn should_drop_preamble_written_before_the_first_marker() {
        let reply = format!("Here is your brief!\n{SECTION_FORM}\nBuilding well.");
        assert!(!parse(&reply).full_prose().contains("Here is your brief"));
    }

    #[test]
    fn should_return_an_empty_brief_for_an_empty_reply() {
        let b = parse("");
        assert!(b.is_empty());
        assert_eq!(b.verdict, CoachVerdict::Proceed);
    }

    #[test]
    fn should_treat_a_section_with_no_body_as_absent() {
        let reply = format!("{SECTION_FORM}\n\n{SECTION_SESSION}\nRide it.");
        let b = parse(&reply);
        assert_eq!(b.form, None);
        assert_eq!(b.session.as_deref(), Some("Ride it."));
    }

    #[test]
    fn should_strip_leaked_thinking_tags_from_the_prose() {
        let reply = format!("{SECTION_FORM}\n<thinking>\nBuilding well.\n</thinking>");
        assert_eq!(parse(&reply).form.as_deref(), Some("Building well."));
    }

    #[test]
    fn should_read_a_marker_wrapped_in_markdown() {
        // Models decorate markers unbidden, and a missed marker is a lost card.
        let reply = format!("**{SECTION_FORM}**\nBuilding well.\n**{MARKER_VERDICT}** EASE");
        let b = parse(&reply);
        assert_eq!(b.form.as_deref(), Some("Building well."));
        assert_eq!(b.verdict, CoachVerdict::Ease);
    }

    #[test]
    fn should_not_start_a_section_from_a_marker_quoted_mid_sentence() {
        let reply = format!("{SECTION_FORM}\nI would normally write ===SESSION=== here, but no.");
        let b = parse(&reply);
        assert!(
            b.form.as_deref().is_some_and(|f| f.contains("but no")),
            "an inline mention belongs to the body: {:?}",
            b.form
        );
        assert_eq!(b.session, None);
    }

    // ── The verdict ──────────────────────────────────────────────────────────

    #[test]
    fn should_read_each_verdict_from_the_trailer() {
        for (text, expected) in [
            ("PROCEED", CoachVerdict::Proceed),
            ("EASE", CoachVerdict::Ease),
            ("REST", CoachVerdict::Rest),
        ] {
            let reply = format!("{SECTION_FORM}\nBody.\n{MARKER_VERDICT} {text}");
            assert_eq!(parse(&reply).verdict, expected, "for {text}");
        }
    }

    #[test]
    fn should_read_a_verdict_case_insensitively() {
        let reply = format!("{SECTION_FORM}\nBody.\n{MARKER_VERDICT} ease");
        assert_eq!(parse(&reply).verdict, CoachVerdict::Ease);
    }

    #[test]
    fn should_treat_a_time_off_verdict_as_rest() {
        let reply = format!("{SECTION_FORM}\nBody.\n{MARKER_VERDICT} TIME_OFF");
        assert_eq!(parse(&reply).verdict, CoachVerdict::Rest);
    }

    #[test]
    fn should_default_to_proceeding_when_no_verdict_marker_is_present() {
        // An unreadable reply must not cancel training the rider was ready for.
        let reply = format!("{SECTION_FORM}\nYour fitness is building nicely.");
        assert_eq!(parse(&reply).verdict, CoachVerdict::Proceed);
    }

    #[test]
    fn should_fall_back_to_keywords_when_the_trailer_is_missing() {
        let reply = format!("{SECTION_SESSION}\nTake a rest day — you have earned it.");
        assert_eq!(parse(&reply).verdict, CoachVerdict::Rest);
    }

    #[test]
    fn should_survive_turkish_text_before_the_verdict_marker() {
        // Case-folding a copy and indexing the original by the folded offset
        // corrupts everything after a Turkish ı — see ai::naming.
        let reply =
            format!("{SECTION_FUELING}\nKahvaltıda yulaf ezmesi ve yoğurt.\n{MARKER_VERDICT} EASE");
        let b = parse(&reply);
        assert_eq!(b.verdict, CoachVerdict::Ease);
        assert_eq!(
            b.fueling.as_deref(),
            Some("Kahvaltıda yulaf ezmesi ve yoğurt.")
        );
    }

    // ── The recommendation, and who is allowed to make one ───────────────────

    #[test]
    fn should_read_a_recommended_workout_when_no_program_owns_the_day() {
        let reply = format!(
            "{SECTION_SESSION}\nRide easy.\n{MARKER_VERDICT} EASE\n{MARKER_RECOMMENDED} Recovery Spin"
        );
        let b = parse_brief(&reply, "2026-08-11", "fp1", false, None);
        assert_eq!(b.recommended_workout.as_deref(), Some("Recovery Spin"));
    }

    #[test]
    fn should_ignore_a_recommended_workout_when_a_program_owns_the_day() {
        // The prompt never asked for one. Acting on it anyway would drop a
        // session out of a progression that never makes it up.
        let reply = format!(
            "{SECTION_SESSION}\nRide easy.\n{MARKER_VERDICT} EASE\n{MARKER_RECOMMENDED} Recovery Spin"
        );
        let b = parse_brief(&reply, "2026-08-11", "fp1", true, Some("Threshold 3x12"));
        assert_eq!(b.recommended_workout, None);
        assert_eq!(
            b.verdict,
            CoachVerdict::Ease,
            "the verdict still stands — only the substitution is refused"
        );
        assert_eq!(b.planned_workout.as_deref(), Some("Threshold 3x12"));
    }

    #[test]
    fn should_treat_a_recommendation_naming_the_planned_session_as_agreement() {
        let reply = format!("{SECTION_SESSION}\nStick with it.\n{MARKER_RECOMMENDED} Endurance 60");
        let b = parse_brief(&reply, "2026-08-11", "fp1", false, Some("Endurance 60"));
        assert_eq!(
            b.recommended_workout, None,
            "no Replace button for a self-swap"
        );
    }

    #[test]
    fn should_match_a_self_swap_across_casing_and_decoration() {
        let reply = format!("{SECTION_SESSION}\nX.\n{MARKER_RECOMMENDED} **endurance 60**");
        let b = parse_brief(&reply, "2026-08-11", "fp1", false, Some("Endurance 60"));
        assert_eq!(b.recommended_workout, None);
    }

    #[test]
    fn should_recommend_nothing_when_the_marker_names_nothing() {
        let reply = format!("{SECTION_SESSION}\nX.\n{MARKER_RECOMMENDED}   ");
        assert_eq!(parse(&reply).recommended_workout, None);
    }
}
