//! Reading workout names back out of a model's reply.
//!
//! Every AI prompt in the app ends with a marker line — `RECOMMENDED_WORKOUT:`,
//! `ALTERNATIVE_WORKOUT:` — whose value is matched against the workout library
//! to decide whether the rider can act on the suggestion. The model does not
//! always reproduce the line cleanly (bold markers, stray punctuation, a
//! typographic × where the library has an x), and a name that fails to match
//! silently costs the rider the Start and Schedule buttons, so both the
//! extraction and the comparison are deliberately forgiving.

/// The value following `marker` on the line that carries it, or `None`.
///
/// Matching is case-insensitive and tolerant of the markdown a model tends to
/// wrap markers in (`**ALTERNATIVE_WORKOUT:** Endurance 90`).
///
/// Works line by line on the original text rather than searching a case-folded
/// copy: case folding is not length-preserving (Turkish `ı` uppercases to `I`,
/// losing a byte), so an offset found in a folded copy can land mid-marker — or
/// mid-character, which panics — when applied back to the original.
pub fn extract_marker_value(text: &str, marker: &str) -> Option<String> {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim_start().trim_start_matches(['*', '#', '-', ' ']);
            let head: String = trimmed.chars().take(marker.len()).collect();
            head.eq_ignore_ascii_case(marker)
                .then(|| trimmed[head.len()..].to_string())
        })
        .map(|value| clean_workout_name(&value))
        .filter(|name| !name.is_empty())
}

/// Strip the decoration a model wraps a name in, leaving the name itself.
pub fn clean_workout_name(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| matches!(c, '*' | '`' | '"' | '\'' | ':' | '-' | '.' | ' '))
        .trim()
        .to_string()
}

/// Whether two workout names refer to the same workout.
///
/// Compares only the alphanumerics, case-folded, so the library's `VO₂Max Long`
/// still matches a model writing `VO2 Max Long`, and `Sweet Spot 2x20` matches
/// `Sweet Spot 2×20`.
pub fn names_match(a: &str, b: &str) -> bool {
    fn key(s: &str) -> String {
        s.chars()
            // Folded before the filter, not after: `×` is a maths symbol rather
            // than alphanumeric, so filtering first would drop it entirely and
            // make `2×20` and `2x20` differ.
            .map(|c| match c {
                '₂' | '²' => '2',
                '×' => 'x',
                other => other,
            })
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    }
    !a.trim().is_empty() && key(a) == key(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_extract_a_plain_marker_value() {
        let text = "DECISION: MODIFY\nALTERNATIVE_WORKOUT: Sweet Spot 2x20\nReasoning";
        assert_eq!(
            extract_marker_value(text, "ALTERNATIVE_WORKOUT:"),
            Some("Sweet Spot 2x20".to_string())
        );
    }

    #[test]
    fn should_survive_a_turkish_place_name_earlier_in_the_reply() {
        // The bug this guards: searching a case-folded copy for the marker and
        // slicing the original at that offset. "Kadıköy" loses a byte when
        // uppercased, so the value came back as ": 2x15 Tempo" — which matches
        // no workout, and the rider lost the Start button.
        let text = "Ride in Kadıköy today.\nALTERNATIVE_WORKOUT: 2x15 Tempo\nReasoning";
        assert_eq!(
            extract_marker_value(text, "ALTERNATIVE_WORKOUT:"),
            Some("2x15 Tempo".to_string())
        );
    }

    #[test]
    fn should_extract_a_marker_the_model_wrapped_in_markdown() {
        let text = "**RECOMMENDED_WORKOUT:** **Endurance 90**";
        assert_eq!(
            extract_marker_value(text, "RECOMMENDED_WORKOUT:"),
            Some("Endurance 90".to_string())
        );
    }

    #[test]
    fn should_return_none_without_the_marker() {
        assert!(extract_marker_value("DECISION: PROCEED", "ALTERNATIVE_WORKOUT:").is_none());
    }

    #[test]
    fn should_return_none_when_the_marker_carries_no_name() {
        assert!(
            extract_marker_value("ALTERNATIVE_WORKOUT:   \n", "ALTERNATIVE_WORKOUT:").is_none()
        );
    }

    #[test]
    fn should_take_only_the_marker_line() {
        let text = "RECOMMENDED_WORKOUT: Endurance 90\nAnd then some prose.";
        assert_eq!(
            extract_marker_value(text, "RECOMMENDED_WORKOUT:"),
            Some("Endurance 90".to_string())
        );
    }

    #[test]
    fn should_match_names_across_typographic_differences() {
        assert!(names_match("VO₂Max Long", "VO2 Max Long"));
        assert!(names_match("Sweet Spot 2x20", "Sweet Spot 2×20"));
        assert!(names_match("endurance 90", "Endurance 90"));
        assert!(names_match("**Threshold 4x8**", "Threshold 4x8"));
    }

    #[test]
    fn should_not_match_different_workouts() {
        assert!(!names_match("Endurance 90", "Endurance 105"));
        assert!(!names_match("Tempo 20", "Tempo 30"));
        assert!(!names_match("", "Endurance 90"));
    }
}
