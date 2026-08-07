/// Convert a subset of Markdown to Pango markup for use with `gtk::Label::set_markup()`.
///
/// Supported: `## / ### headings`, `**bold**`, `*italic*`, `- bullet lists`, `--- hr`, tables.
pub fn to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 256);
    let mut prev_blank = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Horizontal rules → blank line separator
        if matches!(trimmed, "---" | "***" | "___") {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
            continue;
        }

        // Markdown table separator rows (|---|---|) → skip entirely
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let inner = &trimmed[1..trimmed.len() - 1];
            if inner
                .chars()
                .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
            {
                continue;
            }
            // Table data row → render columns joined with a bullet separator
            let cols: Vec<&str> = inner
                .split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !cols.is_empty() {
                let first = cols[0];
                let rest = &cols[1..];
                out.push_str("  ");
                out.push_str(&inline(first));
                for col in rest {
                    out.push_str("  ·  ");
                    out.push_str(&inline(col));
                }
                out.push('\n');
                prev_blank = false;
            }
            continue;
        }

        // ATX headings
        if let Some(rest) = trimmed.strip_prefix("### ") {
            out.push_str("<b>");
            out.push_str(&inline(rest));
            out.push_str("</b>\n");
            prev_blank = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            out.push_str("<span weight=\"bold\" size=\"large\">");
            out.push_str(&inline(rest));
            out.push_str("</span>\n");
            prev_blank = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            out.push_str("<span weight=\"bold\" size=\"x-large\">");
            out.push_str(&inline(rest));
            out.push_str("</span>\n");
            prev_blank = false;
            continue;
        }

        // Unordered list items (either `- ` or `* ` prefix)
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str("  • ");
            out.push_str(&inline(rest));
            out.push('\n');
            prev_blank = false;
            continue;
        }

        // Empty line
        if trimmed.is_empty() {
            out.push('\n');
            prev_blank = true;
            continue;
        }

        // Plain paragraph line
        out.push_str(&inline(line));
        out.push('\n');
        prev_blank = false;
    }

    out.trim_end().to_string()
}

/// Strip all markdown formatting from a string, returning plain text.
/// Used for truncated previews where pango markup is not appropriate.
pub fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim();
        // Skip table separator lines
        if t.starts_with('|') && t.ends_with('|') {
            let inner = &t[1..t.len() - 1];
            if inner
                .chars()
                .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
            {
                continue;
            }
        }
        // Strip leading # / bullet markers
        let stripped = t
            .trim_start_matches('#')
            .trim_start_matches('-')
            .trim_start_matches('*')
            .trim_start_matches('|')
            .trim();
        // Remove inline ** and * markers
        let stripped = stripped.replace("**", "").replace('*', "");
        if !stripped.is_empty() {
            out.push_str(&stripped);
            out.push(' ');
        }
    }
    out.trim_end().to_string()
}

/// XML-escape then apply **bold** and *italic* inline formatting.
fn inline(s: &str) -> String {
    apply_inline_markup(&xml_escape(s))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Walk the string turning `**…**` into `<b>…</b>` and `*…*` into `<i>…</i>`.
fn apply_inline_markup(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut remaining = s;

    while !remaining.is_empty() {
        match remaining.find('*') {
            None => {
                result.push_str(remaining);
                break;
            }
            Some(pos) => {
                result.push_str(&remaining[..pos]);
                remaining = &remaining[pos..];

                if remaining.starts_with("**") {
                    let after = &remaining[2..];
                    if let Some(close) = after.find("**") {
                        result.push_str("<b>");
                        result.push_str(&after[..close]);
                        result.push_str("</b>");
                        remaining = &after[close + 2..];
                        continue;
                    }
                    // No closing ** — emit literally
                    result.push_str("**");
                    remaining = &remaining[2..];
                } else {
                    let after = &remaining[1..];
                    if let Some(close) = single_star_close(after) {
                        result.push_str("<i>");
                        result.push_str(&after[..close]);
                        result.push_str("</i>");
                        remaining = &after[close + 1..];
                        continue;
                    }
                    result.push('*');
                    remaining = &remaining[1..];
                }
            }
        }
    }

    result
}

/// Find the byte index of a closing single `*` that is not part of `**`.
fn single_star_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'*' {
            let next = bytes.get(i + 1) == Some(&b'*');
            let prev = i > 0 && bytes[i - 1] == b'*';
            if !next && !prev {
                return Some(i);
            }
            if next {
                i += 2; // skip **
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Longest preview shown for an AI insight, in characters.
const PREVIEW_MAX_CHARS: usize = 120;

/// First sentence of an AI insight as plain text, for a one-line preview.
///
/// Returns an empty string when there is nothing to preview. Truncation counts
/// characters and cuts on a character boundary — the models answer in the
/// rider's own language, and slicing by byte would panic mid-codepoint.
pub fn insight_preview(insight: &str) -> String {
    if insight.is_empty() {
        return String::new();
    }
    let plain = strip_markdown(insight);
    let first_sentence = plain
        .split(['.', '\n'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if first_sentence.is_empty() {
        String::new()
    } else if first_sentence.chars().count() > PREVIEW_MAX_CHARS {
        let cut = first_sentence
            .char_indices()
            .nth(PREVIEW_MAX_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(first_sentence.len());
        format!("{}…", &first_sentence[..cut])
    } else {
        format!("{}.", first_sentence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_preview_nothing_for_an_empty_insight() {
        assert_eq!(insight_preview(""), "");
        assert_eq!(insight_preview("   \n  "), "");
    }

    #[test]
    fn should_preview_the_first_sentence_with_markdown_stripped() {
        assert_eq!(
            insight_preview("**Form is good.** You can push on."),
            "Form is good."
        );
    }

    #[test]
    fn should_truncate_a_long_sentence_with_an_ellipsis() {
        let long = "a".repeat(200);
        let out = insight_preview(&long);
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS + 1, "120 plus the …");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn should_cut_on_a_character_boundary_not_a_byte_boundary() {
        // Multi-byte text would panic if the cut were taken by byte offset.
        let long = "é".repeat(200);
        let out = insight_preview(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS + 1);
    }

    #[test]
    fn bold_renders_correctly() {
        assert_eq!(inline_markup("**hello**"), "<b>hello</b>");
    }

    #[test]
    fn italic_renders_correctly() {
        assert_eq!(inline_markup("*world*"), "<i>world</i>");
    }

    #[test]
    fn mixed_inline() {
        assert_eq!(
            inline_markup("**bold** and *italic*"),
            "<b>bold</b> and <i>italic</i>"
        );
    }

    #[test]
    fn xml_chars_are_escaped() {
        assert_eq!(inline_markup("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }

    #[test]
    fn heading_renders() {
        assert!(to_pango("## Readiness").contains("large"));
        assert!(to_pango("### Sub").contains("<b>"));
    }

    #[test]
    fn bullet_converts_to_bullet_char() {
        assert!(to_pango("- item one").contains("• item one"));
    }

    #[test]
    fn horizontal_rule_becomes_blank_line() {
        let out = to_pango("before\n---\nafter");
        assert!(!out.contains("---"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    // expose inline_markup for tests
    fn inline_markup(s: &str) -> String {
        apply_inline_markup(&xml_escape(s))
    }
}
