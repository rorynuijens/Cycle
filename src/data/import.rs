use anyhow::{bail, Context, Result};

use crate::data::workout::{Segment, Workout, WorkoutCategory};

// ── ZWO (Zwift workout XML) ───────────────────────────────────────────────────

const MAX_IMPORT_BYTES: usize = 1_048_576; // 1 MB

// Bounds on what a file may expand *to*, which the size cap above does not give
// us (CLAUDE.md §5.3). A single 60-byte `<IntervalsT Repeat="4000000000"/>` asks
// for eight billion segments, each carrying a heap-allocated label — the parse
// dies on memory long before the 1 MB limit is anywhere near relevant. Every
// import is checked against these before a `Workout` exists.
/// Generous ceiling on segment count: a 30×30s over-under set is ~120 segments.
const MAX_SEGMENTS: usize = 2_000;
/// No single interval in a real workout runs longer than this.
const MAX_SEGMENT_SECS: u32 = 6 * 60 * 60;
/// Longest total a structured workout may declare. Also keeps the duration sum
/// in [`build_workout`] clear of overflowing the `u32` it is accumulated into.
const MAX_WORKOUT_SECS: u32 = 24 * 60 * 60;

/// Parse a `.zwo` file (Zwift XML format) into a `Workout`.
pub fn parse_zwo(content: &str) -> Result<Workout> {
    anyhow::ensure!(
        content.len() <= MAX_IMPORT_BYTES,
        "workout file too large (> 1 MB)"
    );
    let name = xml_text(content, "name").unwrap_or_else(|| "Imported Workout".to_string());
    let description = xml_text(content, "description").unwrap_or_default();

    let start = content
        .find("<workout>")
        .context("<workout> element not found")?;
    let end = content.find("</workout>").context("</workout> not found")?;
    let body = &content[start + 9..end]; // skip "<workout>"

    let mut segments: Vec<Segment> = Vec::new();
    let mut pos = 0;

    while pos < body.len() {
        let Some(rel) = body[pos..].find('<') else {
            break;
        };
        let abs = pos + rel;
        let tag_end = body[abs..]
            .find('>')
            .map(|e| abs + e + 1)
            .unwrap_or(body.len());
        let tag = &body[abs..tag_end];
        pos = tag_end;

        if tag.starts_with("</") {
            continue;
        }

        let tag_name = tag
            .trim_start_matches('<')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");

        match tag_name {
            "Warmup" => {
                let dur = attr_u32(tag, "Duration").unwrap_or(300);
                let lo = attr_f32(tag, "PowerLow").unwrap_or(0.4) * 100.0;
                let hi = attr_f32(tag, "PowerHigh").unwrap_or(0.75) * 100.0;
                segments.push(Segment::ramp(dur, lo, hi, "Warm-up"));
            }
            "Cooldown" => {
                let dur = attr_u32(tag, "Duration").unwrap_or(300);
                let lo = attr_f32(tag, "PowerLow").unwrap_or(0.25) * 100.0;
                let hi = attr_f32(tag, "PowerHigh").unwrap_or(0.75) * 100.0;
                // Cooldown ramps down, so high→low
                segments.push(Segment::ramp(dur, hi, lo, "Cool-down"));
            }
            "Ramp" => {
                let dur = attr_u32(tag, "Duration").unwrap_or(300);
                let lo = attr_f32(tag, "PowerLow").unwrap_or(0.5) * 100.0;
                let hi = attr_f32(tag, "PowerHigh").unwrap_or(1.0) * 100.0;
                segments.push(Segment::ramp(dur, lo, hi, "Ramp"));
            }
            "SteadyState" => {
                let dur = attr_u32(tag, "Duration").unwrap_or(300);
                let pwr = attr_f32(tag, "Power")
                    .or_else(|| attr_f32(tag, "PowerLow"))
                    .unwrap_or(0.75)
                    * 100.0;
                segments.push(Segment::steady(dur, pwr, "Steady State"));
            }
            "IntervalsT" => {
                let repeat = attr_u32(tag, "Repeat").unwrap_or(1);
                let on_dur = attr_u32(tag, "OnDuration").unwrap_or(60);
                let off_dur = attr_u32(tag, "OffDuration").unwrap_or(60);
                let on_pwr = attr_f32(tag, "OnPower").unwrap_or(1.05) * 100.0;
                let off_pwr = attr_f32(tag, "OffPower").unwrap_or(0.5) * 100.0;
                // This is the one tag that multiplies, so it has to be refused
                // before the loop rather than trimmed after it: by then the
                // memory has already been asked for.
                anyhow::ensure!(
                    u64::from(repeat) * 2 <= MAX_SEGMENTS.saturating_sub(segments.len()) as u64,
                    "workout expands to too many segments (IntervalsT Repeat=\"{repeat}\")"
                );
                for i in 1..=repeat {
                    segments.push(Segment::steady(on_dur, on_pwr, &format!("Interval {i}")));
                    segments.push(Segment::steady(off_dur, off_pwr, "Recovery"));
                }
            }
            "FreeRide" => {
                let dur = attr_u32(tag, "Duration").unwrap_or(300);
                segments.push(Segment::steady(dur, 60.0, "Free Ride"));
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        bail!("No workout segments found in ZWO file");
    }

    build_workout(name, description, segments)
}

// ── ERG / MRC ─────────────────────────────────────────────────────────────────

/// Parse a `.erg` or `.mrc` file into a `Workout`.
/// ERG uses absolute watts; MRC uses percentage of FTP.
pub fn parse_erg(content: &str) -> Result<Workout> {
    anyhow::ensure!(
        content.len() <= MAX_IMPORT_BYTES,
        "workout file too large (> 1 MB)"
    );
    let mut name = "Imported Workout".to_string();
    let mut description = String::new();
    let mut pairs: Vec<(f32, f32)> = Vec::new();
    let mut in_header = false;
    let mut in_data = false;
    let mut is_mrc = false;

    for line in content.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();

        if lower == "[course header]" {
            in_header = true;
            in_data = false;
            continue;
        }
        if lower == "[end course header]" {
            in_header = false;
            continue;
        }
        if lower == "[course data]" {
            in_data = true;
            in_header = false;
            continue;
        }
        if lower == "[end course data]" {
            in_data = false;
            continue;
        }

        if in_header {
            let (key, val) = split_kv(line);
            match key.to_ascii_lowercase().trim() {
                "description" => description = val.to_string(),
                "file name" => {
                    name = val
                        .trim_end_matches(".erg")
                        .trim_end_matches(".mrc")
                        .to_string()
                }
                "minutes" if val.to_ascii_lowercase().contains("percent") => is_mrc = true,
                _ => {}
            }
        }

        if in_data {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let (Ok(m), Ok(v)) = (parts[0].parse::<f32>(), parts[1].parse::<f32>()) {
                    pairs.push((m, v));
                }
            }
        }
    }

    if pairs.is_empty() {
        bail!("No data points found in ERG/MRC file");
    }

    let mut segments: Vec<Segment> = Vec::new();
    for i in 0..pairs.len().saturating_sub(1) {
        let (t0, v0) = pairs[i];
        let (t1, v1) = pairs[i + 1];
        let dur = ((t1 - t0) * 60.0).round() as u32;
        if dur == 0 {
            continue;
        }
        // ERG: watts → convert assuming 250 W FTP as a neutral reference.
        // MRC: already percentage.
        let (lo, hi) = if is_mrc {
            (v0, v1)
        } else {
            (v0 / 2.5, v1 / 2.5)
        };
        if (hi - lo).abs() < 2.0 {
            segments.push(Segment::steady(dur, lo, ""));
        } else {
            segments.push(Segment::ramp(dur, lo, hi, ""));
        }
    }

    if segments.is_empty() {
        bail!("Could not build segments from ERG data");
    }

    build_workout(name, description, segments)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Assemble a parsed file into a `Workout`, refusing anything outside the
/// bounds a real workout stays inside.
///
/// Every import funnels through here, so this is where the guard rails live: a
/// parser cannot forget to apply them, and nothing that fails them is ever
/// handed to the rest of the app. The duration is summed into a `u64` first
/// because the whole point is that the numbers in the file are untrusted.
fn build_workout(name: String, description: String, segments: Vec<Segment>) -> Result<Workout> {
    anyhow::ensure!(
        segments.len() <= MAX_SEGMENTS,
        "workout has too many segments ({}, limit {MAX_SEGMENTS})",
        segments.len()
    );
    if let Some(longest) = segments.iter().find(|s| s.duration_secs > MAX_SEGMENT_SECS) {
        bail!(
            "workout has a {}s segment, longer than the {MAX_SEGMENT_SECS}s limit",
            longest.duration_secs
        );
    }
    let total: u64 = segments.iter().map(|s| u64::from(s.duration_secs)).sum();
    anyhow::ensure!(
        total <= u64::from(MAX_WORKOUT_SECS),
        "workout is {total}s long, over the {MAX_WORKOUT_SECS}s limit"
    );

    let duration_secs = total as u32;
    let tss = estimate_tss(&segments, duration_secs);
    let category = guess_category(&segments);
    Ok(Workout {
        id: 0,
        name,
        description,
        duration_secs,
        tss,
        category,
        segments,
    })
}

fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn attr_f32(tag: &str, attr: &str) -> Option<f32> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    tag[start..end].parse().ok()
}

fn attr_u32(tag: &str, attr: &str) -> Option<u32> {
    attr_f32(tag, attr).map(|v| v.round() as u32)
}

fn split_kv(line: &str) -> (&str, &str) {
    if let Some(pos) = line.find('=') {
        (line[..pos].trim(), line[pos + 1..].trim())
    } else {
        (line, "")
    }
}

fn estimate_tss(segments: &[Segment], duration_secs: u32) -> f32 {
    if duration_secs == 0 {
        return 0.0;
    }
    let weighted: f32 = segments
        .iter()
        .map(|s| {
            let mid = (s.power_low_pct + s.power_high_pct) / 2.0 / 100.0;
            mid.powi(2) * s.duration_secs as f32
        })
        .sum();
    let if_ = (weighted / duration_secs as f32).sqrt();
    if_ * (duration_secs as f32 / 3600.0) * 100.0
}

fn guess_category(segments: &[Segment]) -> WorkoutCategory {
    let total: u32 = segments.iter().map(|s| s.duration_secs).sum();
    if total == 0 {
        return WorkoutCategory::Custom;
    }
    let avg_pct: f32 = segments
        .iter()
        .map(|s| (s.power_low_pct + s.power_high_pct) / 2.0 * s.duration_secs as f32)
        .sum::<f32>()
        / total as f32;
    match avg_pct as u32 {
        0..=55 => WorkoutCategory::Recovery,
        56..=75 => WorkoutCategory::Endurance,
        76..=90 => WorkoutCategory::Tempo,
        91..=97 => WorkoutCategory::SweetSpot,
        98..=105 => WorkoutCategory::Threshold,
        106..=120 => WorkoutCategory::Vo2Max,
        _ => WorkoutCategory::Anaerobic,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_basic_zwo() {
        let zwo = r#"
            <workout_file>
                <name>Test Workout</name>
                <workout>
                    <Warmup Duration="300" PowerLow="0.4" PowerHigh="0.75"/>
                    <SteadyState Duration="600" Power="0.9"/>
                    <Cooldown Duration="300" PowerHigh="0.75" PowerLow="0.25"/>
                </workout>
            </workout_file>"#;
        let w = parse_zwo(zwo).unwrap();
        assert_eq!(w.name, "Test Workout");
        assert_eq!(w.segments.len(), 3);
        assert_eq!(w.duration_secs, 1200);
    }

    #[test]
    fn should_expand_intervals_t() {
        let zwo = r#"
            <workout_file>
                <name>Intervals</name>
                <workout>
                    <IntervalsT Repeat="3" OnDuration="60" OffDuration="60" OnPower="1.2" OffPower="0.5"/>
                </workout>
            </workout_file>"#;
        let w = parse_zwo(zwo).unwrap();
        assert_eq!(w.segments.len(), 6); // 3 × (on + off)
        assert_eq!(w.duration_secs, 360);
    }

    // ── guard rails on untrusted files (CLAUDE.md §5.3) ──────────────────────

    /// Wrap `body` in the minimum ZWO scaffolding the parser needs.
    fn zwo(body: &str) -> String {
        format!("<workout_file><name>t</name><workout>{body}</workout></workout_file>")
    }

    #[test]
    fn should_refuse_an_intervals_repeat_that_would_exhaust_memory() {
        // 158 bytes asking for 8.6 billion segments. The file is nowhere near
        // the 1 MB limit, which is exactly why size alone cannot be the guard.
        let file = zwo(
            r#"<IntervalsT Repeat="4294967295" OnDuration="60" OffDuration="60" OnPower="1.05" OffPower="0.5"/>"#,
        );
        assert!(file.len() < MAX_IMPORT_BYTES);
        let err = parse_zwo(&file).expect_err("an unbounded Repeat must be refused");
        assert!(
            err.to_string().contains("too many segments"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn should_refuse_a_repeat_just_past_the_segment_limit() {
        // Each repeat is two segments, so the last accepted Repeat is half the cap.
        let ok = zwo(&format!(
            r#"<IntervalsT Repeat="{}" OnDuration="1" OffDuration="1"/>"#,
            MAX_SEGMENTS / 2
        ));
        assert_eq!(parse_zwo(&ok).unwrap().segments.len(), MAX_SEGMENTS);

        let over = zwo(&format!(
            r#"<IntervalsT Repeat="{}" OnDuration="1" OffDuration="1"/>"#,
            MAX_SEGMENTS / 2 + 1
        ));
        assert!(parse_zwo(&over).is_err());
    }

    #[test]
    fn should_count_repeats_against_segments_already_parsed() {
        // The budget is what is left, not the whole cap: two tags that each fit
        // on their own must still be refused together.
        let half = MAX_SEGMENTS / 4;
        let file = zwo(&format!(
            r#"<IntervalsT Repeat="{half}" OnDuration="1" OffDuration="1"/>
               <IntervalsT Repeat="{}" OnDuration="1" OffDuration="1"/>"#,
            MAX_SEGMENTS / 2
        ));
        assert!(parse_zwo(&file).is_err());
    }

    #[test]
    fn should_refuse_a_single_absurdly_long_segment() {
        let file = zwo(r#"<SteadyState Duration="4000000000" Power="0.75"/>"#);
        let err = parse_zwo(&file).expect_err("a 127-year segment must be refused");
        assert!(err.to_string().contains("longer than"), "unexpected: {err}");
    }

    #[test]
    fn should_refuse_a_workout_longer_than_a_day() {
        // Each segment is under the per-segment cap; only the total is over it.
        let seg = r#"<SteadyState Duration="18000" Power="0.75"/>"#; // 5 h
        let file = zwo(&seg.repeat(6)); // 30 h
        let err = parse_zwo(&file).expect_err("a 30-hour workout must be refused");
        assert!(err.to_string().contains("over the"), "unexpected: {err}");
    }

    #[test]
    fn should_still_accept_a_long_but_realistic_workout() {
        // A 3 h endurance ride with a warm-up is ordinary and must survive.
        let file = zwo(r#"<Warmup Duration="900" PowerLow="0.4" PowerHigh="0.75"/>
               <SteadyState Duration="9900" Power="0.65"/>
               <Cooldown Duration="600" PowerHigh="0.6" PowerLow="0.3"/>"#);
        let w = parse_zwo(&file).expect("a 3 h ride is a normal workout");
        assert_eq!(w.duration_secs, 11_400);
    }

    #[test]
    fn should_parse_erg_file() {
        let erg = "[COURSE HEADER]\nDESCRIPTION = My Ride\nMINUTES WATTS\n[END COURSE HEADER]\n\
                   [COURSE DATA]\n0.00 100\n5.00 200\n10.00 100\n[END COURSE DATA]";
        let w = parse_erg(erg).unwrap();
        assert_eq!(w.segments.len(), 2);
        assert_eq!(w.duration_secs, 600);
    }

    #[test]
    fn should_return_error_for_empty_zwo() {
        assert!(parse_zwo("<workout_file><workout></workout></workout_file>").is_err());
    }
}
