//! The ride card: a picture of a ride to attach beside its exported FIT file.
//!
//! FIT cannot carry an image and Garmin Connect has no photo API, so the rider
//! uploads the FIT as before and attaches this PNG to the activity by hand. It
//! draws the one thing Connect never can, because Connect never had the plan:
//! the targets the rider was set, with the power they actually rode over them.
//!
//! The plan comes from the ride itself — every recorded second carries the
//! target in force (`DataPoint::target_watts`) — so no workout has to be found.
//! Only a workout gets a card ([`has_plan`]): a route ride has its map on
//! Connect already, and the rider wants no card for it (2026-10-12). The
//! renderer still copes with a planless ride, drawing the trace alone.
//!
//! Drawn with plain Cairo into an image surface, so it needs no window and no
//! GTK, and the tests can render a real card.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;

use crate::data::athlete::{power_zone_index, ZONE_COLORS};
use crate::data::session::{DataPoint, Session};

pub const CARD_WIDTH: i32 = 1600;
pub const CARD_HEIGHT: i32 = 900;

// A PNG has no theme to follow, so the card fixes one look: dark (rider's
// choice, 2026-10-12). These are libadwaita's own dark-theme window background
// and foreground, so the card reads as the app it came from. They are the only
// colours here that are not zone colours (CLAUDE.md §1.6), and they exist only
// because an image file cannot ask the style manager.
const BG: (f64, f64, f64) = (0.133, 0.133, 0.149); // Adwaita dark #222226
const FG: (f64, f64, f64) = (1.0, 1.0, 1.0);

const FONT: &str = "Adwaita Sans";
const MARGIN: f64 = 64.0;
/// Chart width per point of the power trace — see `draw_chart`.
const TRACE_PX_PER_POINT: f64 = 4.0;

/// Whether the ride followed a plan, and so gets a card. A route ride or an
/// imported file recorded no targets.
pub fn has_plan(session: &Session) -> bool {
    session.data_points.iter().any(|p| p.target_watts.is_some())
}

/// Where the card goes: beside the FIT, with the same name.
pub fn card_path(fit_path: &Path) -> PathBuf {
    fit_path.with_extension("png")
}

/// One run of seconds that all had the same target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetBlock {
    /// First second of the run.
    pub from_secs: u32,
    /// One past the last second of the run.
    pub to_secs: u32,
    pub watts: u32,
}

/// The plan as it was ridden, read back off the recorded targets.
///
/// Consecutive seconds with the same target are one block. A second with no
/// target — past the last segment, or a gap in the recording — ends a block
/// rather than being drawn as a target of zero.
pub fn target_blocks(points: &[DataPoint]) -> Vec<TargetBlock> {
    let mut blocks: Vec<TargetBlock> = Vec::new();
    for p in points {
        let Some(watts) = p.target_watts else {
            continue;
        };
        match blocks.last_mut() {
            Some(b) if b.watts == watts && b.to_secs == p.elapsed_secs => {
                b.to_secs = p.elapsed_secs + 1;
            }
            _ => blocks.push(TargetBlock {
                from_secs: p.elapsed_secs,
                to_secs: p.elapsed_secs + 1,
                watts,
            }),
        }
    }
    blocks
}

/// Power averaged into `columns` equal slices of `total_secs`, one per column
/// of the chart. Averaging is also the smoothing: a one-second spike would
/// otherwise be drawn as a spike a pixel wide. A slice with no power reading
/// is `None`, and is drawn as a gap, not a drop to zero.
pub fn power_columns(points: &[DataPoint], total_secs: u32, columns: usize) -> Vec<Option<u32>> {
    if total_secs == 0 || columns == 0 {
        return Vec::new();
    }
    let mut sums = vec![(0u64, 0u64); columns];
    for p in points {
        let Some(w) = p.power_watts else {
            continue;
        };
        if p.elapsed_secs >= total_secs {
            continue;
        }
        let col = (p.elapsed_secs as u64 * columns as u64 / total_secs as u64) as usize;
        let slot = &mut sums[col.min(columns - 1)];
        slot.0 += w as u64;
        slot.1 += 1;
    }
    sums.into_iter()
        .map(|(sum, n)| (n > 0).then(|| (sum / n) as u32))
        .collect()
}

/// A duration as the card shows it: `1:05:00`, or `45:00` under the hour.
pub fn format_ride_time(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The numbers along the bottom, as `(caption, value)`. Only those the ride
/// has: a ride with no heart-rate strap gets no heart-rate tile, rather than
/// a dash taking up a sixth of the card.
pub fn stat_tiles(session: &Session, fallback_ftp: u32) -> Vec<(&'static str, String)> {
    let mut tiles = vec![("Time", format_ride_time(session.duration_secs()))];
    if let Some(avg) = session.average_power() {
        tiles.push(("Avg Power", format!("{} W", avg.round() as u32)));
    }
    if let Some(np) = session.normalised_power() {
        tiles.push(("NP", format!("{} W", np.round() as u32)));
        let ftp = session.ftp_watts.unwrap_or(fallback_ftp);
        if ftp > 0 {
            tiles.push(("IF", format!("{:.2}", np / ftp as f32)));
        }
    }
    if let Some(tss) = session.tss(fallback_ftp) {
        tiles.push(("TSS", format!("{}", tss.round() as u32)));
    }
    if let Some(hr) = session.average_hr() {
        tiles.push(("Avg HR", format!("{} bpm", hr.round() as u32)));
    }
    tiles
}

/// Render the card for `session` and write it to `path` as a PNG.
///
/// `fallback_ftp` scores a ride that carries no stamped FTP, exactly as the
/// rest of the app does. The title is the name the ride is shown under.
pub fn write_card(path: &Path, session: &Session, title: &str, fallback_ftp: u32) -> Result<()> {
    let surface = render(session, title, fallback_ftp)?;
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    surface
        .write_to_png(&mut file)
        .context("could not encode the ride card")?;
    Ok(())
}

fn render(session: &Session, title: &str, fallback_ftp: u32) -> Result<cairo::ImageSurface> {
    let surface = cairo::ImageSurface::create(cairo::Format::Rgb24, CARD_WIDTH, CARD_HEIGHT)
        .context("could not create the ride card surface")?;
    let cr = cairo::Context::new(&surface).context("could not draw the ride card")?;
    let w = CARD_WIDTH as f64;

    cr.set_source_rgb(BG.0, BG.1, BG.2);
    cr.paint()?;

    // ── Heading: what was ridden, and when ──────────────────────────────────
    cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(56.0);
    cr.set_source_rgb(FG.0, FG.1, FG.2);
    let title = if title.trim().is_empty() {
        "Ride"
    } else {
        title
    };
    let title = fit_text(&cr, title, w - 2.0 * MARGIN)?;
    cr.move_to(MARGIN, MARGIN + 52.0);
    cr.show_text(&title)?;

    cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(30.0);
    cr.set_source_rgba(FG.0, FG.1, FG.2, 0.55);
    let date = session
        .started_at
        .with_timezone(&Local)
        .format("%A %-d %B %Y")
        .to_string();
    cr.move_to(MARGIN, MARGIN + 104.0);
    cr.show_text(&date)?;

    // ── The chart: plan and ride ────────────────────────────────────────────
    let chart_top = 240.0;
    let chart_bottom = 660.0;
    draw_chart(
        &cr,
        session,
        fallback_ftp,
        (
            MARGIN,
            chart_top,
            w - 2.0 * MARGIN,
            chart_bottom - chart_top,
        ),
    )?;

    // ── The numbers ─────────────────────────────────────────────────────────
    let tiles = stat_tiles(session, fallback_ftp);
    if !tiles.is_empty() {
        let tile_w = (w - 2.0 * MARGIN) / tiles.len() as f64;
        for (i, (caption, value)) in tiles.iter().enumerate() {
            let x = MARGIN + i as f64 * tile_w;
            cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(26.0);
            cr.set_source_rgba(FG.0, FG.1, FG.2, 0.55);
            cr.move_to(x, 740.0);
            cr.show_text(caption)?;

            cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            cr.set_font_size(52.0);
            cr.set_source_rgb(FG.0, FG.1, FG.2);
            cr.move_to(x, 804.0);
            cr.show_text(value)?;
        }
    }

    // ── Where it came from ──────────────────────────────────────────────────
    cr.select_font_face(FONT, cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(22.0);
    cr.set_source_rgba(FG.0, FG.1, FG.2, 0.35);
    let mark = "Ridden with Cycle";
    let ext = cr.text_extents(mark)?;
    cr.move_to(w - MARGIN - ext.x_advance(), CARD_HEIGHT as f64 - 36.0);
    cr.show_text(mark)?;

    drop(cr);
    surface.flush();
    Ok(surface)
}

/// The workout's target blocks in zone colour, with the ridden power over the
/// top in the foreground colour — the same picture as the in-app graph.
fn draw_chart(
    cr: &cairo::Context,
    session: &Session,
    fallback_ftp: u32,
    (x0, y0, cw, ch): (f64, f64, f64, f64),
) -> Result<()> {
    let points = &session.data_points;
    let total_secs = points.iter().map(|p| p.elapsed_secs + 1).max().unwrap_or(0);
    if total_secs == 0 {
        return Ok(());
    }
    let ftp = session.ftp_watts.unwrap_or(fallback_ftp);
    let blocks = target_blocks(points);
    // One point every few pixels: at a pixel each, an hour is two seconds a
    // point and the line is all second-to-second noise.
    let columns = power_columns(points, total_secs, (cw / TRACE_PX_PER_POINT) as usize);

    // 125 % FTP fills the chart, as in the app — but a sprint or a ramp test
    // past that would be cut off on a card that cannot be scrolled, so the
    // scale stretches to the highest thing it has to show.
    let highest = columns
        .iter()
        .flatten()
        .copied()
        .chain(blocks.iter().map(|b| b.watts))
        .max()
        .unwrap_or(0);
    let max_watts = (ftp as f64 * 1.25).max(highest as f64 * 1.05).max(1.0);
    let x_at = |secs: f64| x0 + secs / total_secs as f64 * cw;
    let y_at = |watts: f64| y0 + ch - (watts / max_watts).min(1.0) * ch;

    for b in &blocks {
        let (r, g, bl) = ZONE_COLORS[power_zone_index(b.watts, ftp)];
        cr.set_source_rgba(r, g, bl, 0.65);
        // Edges on whole pixels: a warm-up ramp is a block a second, and
        // anti-aliased edges between neighbours show as stripes.
        let x = x_at(b.from_secs as f64).round();
        let y = y_at(b.watts as f64).round();
        cr.rectangle(x, y, x_at(b.to_secs as f64).round() - x, y0 + ch - y);
        cr.fill()?;
    }

    if ftp > 0 {
        let y = y_at(ftp as f64);
        cr.set_source_rgba(FG.0, FG.1, FG.2, 0.25);
        cr.set_line_width(2.0);
        cr.set_dash(&[8.0, 8.0], 0.0);
        cr.move_to(x0, y);
        cr.line_to(x0 + cw, y);
        cr.stroke()?;
        cr.set_dash(&[], 0.0);
    }

    // The trace, one run per stretch of readings so a gap stays a gap.
    let col_x = |i: usize| x0 + (i as f64 + 0.5) / columns.len() as f64 * cw;
    let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for (i, watts) in columns.iter().enumerate() {
        match watts {
            Some(watts) => current.push((col_x(i), y_at(*watts as f64))),
            None if !current.is_empty() => runs.push(std::mem::take(&mut current)),
            None => {}
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    for run in &runs {
        let (Some(first), Some(last)) = (run.first(), run.last()) else {
            continue;
        };
        cr.set_source_rgba(FG.0, FG.1, FG.2, 0.10);
        cr.move_to(first.0, y0 + ch);
        for &(x, y) in run {
            cr.line_to(x, y);
        }
        cr.line_to(last.0, y0 + ch);
        cr.close_path();
        cr.fill()?;

        cr.set_source_rgba(FG.0, FG.1, FG.2, 0.9);
        cr.set_line_width(3.0);
        cr.set_line_join(cairo::LineJoin::Round);
        cr.move_to(first.0, first.1);
        for &(x, y) in &run[1..] {
            cr.line_to(x, y);
        }
        cr.stroke()?;
    }
    Ok(())
}

/// `text`, cut down with an ellipsis until it fits `max_width` in the current
/// font. Cut by characters, so a multi-byte letter is never split.
fn fit_text(cr: &cairo::Context, text: &str, max_width: f64) -> Result<String> {
    if cr.text_extents(text)?.x_advance() <= max_width {
        return Ok(text.to_string());
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let candidate: String = chars[..keep]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
        if cr.text_extents(&candidate)?.x_advance() <= max_width {
            return Ok(candidate);
        }
    }
    Ok("…".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(secs: u32, power: Option<u32>, target: Option<u32>) -> DataPoint {
        DataPoint {
            elapsed_secs: secs,
            power_watts: power,
            target_watts: target,
            heart_rate_bpm: None,
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        }
    }

    // ── Where it is written ─────────────────────────────────────────────────

    #[test]
    fn should_put_the_card_beside_the_fit_with_the_same_name() {
        assert_eq!(
            card_path(Path::new("/home/r/Threshold-2026-10-12.fit")),
            PathBuf::from("/home/r/Threshold-2026-10-12.png")
        );
    }

    #[test]
    fn should_add_an_extension_when_the_rider_typed_none() {
        assert_eq!(
            card_path(Path::new("/home/r/ride")),
            PathBuf::from("/home/r/ride.png")
        );
    }

    // ── Which rides get one ─────────────────────────────────────────────────

    #[test]
    fn should_give_a_workout_a_card() {
        assert!(has_plan(&hour_at(200, None, Some(250))));
    }

    #[test]
    fn should_not_give_a_route_ride_a_card() {
        assert!(!has_plan(&route_hour(vec![Some(200); 3600], 250)));
    }

    #[test]
    fn should_give_a_card_to_a_workout_with_a_single_planned_second() {
        // A workout ended a second into its first block still followed a plan.
        let mut s = route_hour(vec![Some(200); 3600], 250);
        s.data_points[0].target_watts = Some(100);
        assert!(has_plan(&s));
    }

    // ── Reading the plan back ───────────────────────────────────────────────

    #[test]
    fn should_merge_seconds_with_the_same_target_into_one_block() {
        let pts: Vec<_> = (0..3)
            .map(|s| point(s, Some(150), Some(150)))
            .chain((3..5).map(|s| point(s, Some(250), Some(250))))
            .collect();
        assert_eq!(
            target_blocks(&pts),
            vec![
                TargetBlock {
                    from_secs: 0,
                    to_secs: 3,
                    watts: 150
                },
                TargetBlock {
                    from_secs: 3,
                    to_secs: 5,
                    watts: 250
                },
            ]
        );
    }

    #[test]
    fn should_not_draw_a_second_without_a_target_as_zero() {
        let pts = vec![
            point(0, Some(200), Some(200)),
            point(1, Some(200), None),
            point(2, Some(200), Some(200)),
        ];
        // Same target either side of the hole, but not one block across it.
        assert_eq!(
            target_blocks(&pts),
            vec![
                TargetBlock {
                    from_secs: 0,
                    to_secs: 1,
                    watts: 200
                },
                TargetBlock {
                    from_secs: 2,
                    to_secs: 3,
                    watts: 200
                },
            ]
        );
    }

    #[test]
    fn should_split_a_block_across_a_gap_in_the_recording() {
        // Seconds 1–4 missing (an app restart): the same target either side
        // is still two blocks, or the card would claim a plan was ridden
        // through time that was not recorded.
        let pts = vec![
            point(0, Some(200), Some(200)),
            point(5, Some(200), Some(200)),
        ];
        assert_eq!(target_blocks(&pts).len(), 2);
    }

    #[test]
    fn should_find_no_plan_in_a_route_ride() {
        let pts: Vec<_> = (0..60).map(|s| point(s, Some(180), None)).collect();
        assert!(target_blocks(&pts).is_empty());
    }

    // ── The trace ───────────────────────────────────────────────────────────

    #[test]
    fn should_average_each_slice_of_the_ride() {
        let pts = vec![
            point(0, Some(100), None),
            point(1, Some(300), None),
            point(2, Some(50), None),
            point(3, Some(50), None),
        ];
        assert_eq!(power_columns(&pts, 4, 2), vec![Some(200), Some(50)]);
    }

    #[test]
    fn should_leave_a_slice_with_no_readings_empty_not_zero() {
        let pts = vec![point(0, Some(100), None), point(3, Some(100), None)];
        assert_eq!(
            power_columns(&pts, 4, 4),
            vec![Some(100), None, None, Some(100)]
        );
    }

    #[test]
    fn should_put_the_last_second_in_the_last_column() {
        let pts = vec![point(3599, Some(222), None)];
        let cols = power_columns(&pts, 3600, 1472);
        assert_eq!(cols.last(), Some(&Some(222)));
    }

    #[test]
    fn should_ignore_a_second_past_the_end() {
        let pts = vec![point(10, Some(999), None)];
        assert_eq!(power_columns(&pts, 10, 5), vec![None; 5]);
    }

    #[test]
    fn should_draw_nothing_for_an_empty_ride() {
        assert!(power_columns(&[], 0, 100).is_empty());
        assert!(power_columns(&[point(0, Some(1), None)], 1, 0).is_empty());
    }

    // ── The numbers ─────────────────────────────────────────────────────────

    #[test]
    fn should_show_hours_only_once_the_ride_reaches_one() {
        assert_eq!(format_ride_time(3599), "59:59");
        assert_eq!(format_ride_time(3600), "1:00:00");
        assert_eq!(format_ride_time(3905), "1:05:05");
        // Minutes are what is left of the hour, divided: 15, not 900 % 60.
        assert_eq!(format_ride_time(4500), "1:15:00");
        assert_eq!(format_ride_time(0), "0:00");
    }

    fn hour_at(watts: u32, hr: Option<u32>, ftp: Option<u32>) -> Session {
        let mut s = Session::new(None);
        s.ftp_watts = ftp;
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(3600));
        s.data_points = (0..3600)
            .map(|i| DataPoint {
                heart_rate_bpm: hr,
                ..point(i, Some(watts), Some(watts))
            })
            .collect();
        s
    }

    #[test]
    fn should_score_an_hour_at_ftp_as_a_hundred_tss_and_an_if_of_one() {
        let tiles = stat_tiles(&hour_at(250, Some(150), Some(250)), 999);
        let get = |c: &str| tiles.iter().find(|t| t.0 == c).map(|t| t.1.clone());
        assert_eq!(get("Time").as_deref(), Some("1:00:00"));
        assert_eq!(get("Avg Power").as_deref(), Some("250 W"));
        assert_eq!(get("NP").as_deref(), Some("250 W"));
        assert_eq!(get("IF").as_deref(), Some("1.00"));
        assert_eq!(get("TSS").as_deref(), Some("100"));
        assert_eq!(get("Avg HR").as_deref(), Some("150 bpm"));
    }

    #[test]
    fn should_score_against_the_stamped_ftp_not_todays() {
        // Stamped 200, profile now says 400: IF must be read against 200.
        let tiles = stat_tiles(&hour_at(200, None, Some(200)), 400);
        assert!(tiles.contains(&("IF", "1.00".to_string())));
    }

    #[test]
    fn should_fall_back_to_the_profile_ftp_for_an_unstamped_ride() {
        let tiles = stat_tiles(&hour_at(200, None, None), 400);
        assert!(tiles.contains(&("IF", "0.50".to_string())));
    }

    #[test]
    fn should_leave_out_heart_rate_when_there_was_no_strap() {
        let tiles = stat_tiles(&hour_at(200, None, Some(200)), 200);
        assert!(tiles.iter().all(|t| t.0 != "Avg HR"));
    }

    #[test]
    fn should_not_divide_by_a_zero_ftp() {
        let tiles = stat_tiles(&hour_at(200, None, None), 0);
        assert!(tiles.iter().all(|t| t.0 != "IF"));
    }

    // ── The picture ─────────────────────────────────────────────────────────

    fn pixel(surface: &mut cairo::ImageSurface, x: i32, y: i32) -> (u8, u8, u8) {
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface is not shared");
        let i = y as usize * stride + x as usize * 4;
        // Rgb24 is stored native-endian as 0x00RRGGBB.
        (data[i + 2], data[i + 1], data[i])
    }

    #[test]
    fn should_write_a_png_of_the_card_size() {
        let dir = std::env::temp_dir().join(format!("cycle-card-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ride.png");
        write_card(&path, &hour_at(250, Some(150), Some(250)), "Threshold", 250).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let png = cairo::ImageSurface::create_from_png(&mut file).unwrap();
        assert_eq!((png.width(), png.height()), (CARD_WIDTH, CARD_HEIGHT));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_fill_the_plan_in_its_zone_colour() {
        // An hour at FTP is a single Z4 block filling the chart's width, so a
        // point inside it, below the trace, is the Z4 colour over the dark
        // background — not the background itself.
        let session = hour_at(250, None, Some(250));
        let mut surface = render(&session, "Threshold", 250).unwrap();
        let (r, g, b) = pixel(&mut surface, CARD_WIDTH / 2, 640);
        let (zr, zg, zb) = ZONE_COLORS[3];
        let expect = |z: f64, bg: f64| ((z * 0.65 + bg * 0.35) * 255.0).round() as i32;
        // The trace's fill sits over the block too, so allow for it.
        for (got, want) in [
            (r, expect(zr, BG.0)),
            (g, expect(zg, BG.1)),
            (b, expect(zb, BG.2)),
        ] {
            assert!(
                (got as i32 - want).abs() <= 30,
                "got {got}, want about {want}"
            );
        }
    }

    #[test]
    fn should_leave_the_chart_background_where_a_route_ride_has_no_plan() {
        // Route ride at 100 W against FTP 250: the top of the chart is empty.
        let mut session = hour_at(100, None, Some(250));
        for p in &mut session.data_points {
            p.target_watts = None;
        }
        let mut surface = render(&session, "Col du Galibier", 250).unwrap();
        let bg = |c: f64| (c * 255.0).round() as u8;
        assert_eq!(
            pixel(&mut surface, CARD_WIDTH / 2, 260),
            (bg(BG.0), bg(BG.1), bg(BG.2))
        );
    }

    /// A planless ride (a route) at `watts` for an hour.
    fn route_hour(watts: Vec<Option<u32>>, ftp: u32) -> Session {
        let mut s = hour_at(0, None, Some(ftp));
        for (p, w) in s.data_points.iter_mut().zip(watts) {
            p.power_watts = w;
            p.target_watts = None;
        }
        s
    }

    fn brightness(px: (u8, u8, u8)) -> i32 {
        px.0 as i32 + px.1 as i32 + px.2 as i32
    }

    const BG_BRIGHTNESS: i32 = 34 + 34 + 38;

    #[test]
    fn should_draw_the_trace_at_its_height_on_the_ftp_scale() {
        // Chart 240–660 px, 125 % of FTP 250 = 312.5 W at the top: 200 W sits
        // at 660 − 420 × 0.64 = 391 px.
        let session = route_hour(vec![Some(200); 3600], 250);
        let mut surface = render(&session, "Steady", 250).unwrap();
        let x = CARD_WIDTH / 2;
        assert!(
            brightness(pixel(&mut surface, x, 391)) > 600,
            "the line itself"
        );
        assert_eq!(
            brightness(pixel(&mut surface, x, 380)),
            BG_BRIGHTNESS,
            "above it"
        );
        let under = brightness(pixel(&mut surface, x, 420));
        assert!(
            under > BG_BRIGHTNESS && under < 200,
            "the faint fill below, got {under}"
        );
        // The fill stops at the chart's foot rather than running on into the
        // numbers underneath.
        assert_eq!(
            brightness(pixel(&mut surface, x, 680)),
            BG_BRIGHTNESS,
            "below the chart"
        );
    }

    #[test]
    fn should_place_each_block_where_it_was_ridden() {
        // First half at 150 W (Z2 against 250), second half at 250 W (Z4).
        let mut session = hour_at(0, None, Some(250));
        for p in &mut session.data_points {
            let w = if p.elapsed_secs < 1800 { 150 } else { 250 };
            p.target_watts = Some(w);
            p.power_watts = None;
        }
        let mut surface = render(&session, "Two blocks", 250).unwrap();
        let chart_w = CARD_WIDTH as f64 - 2.0 * MARGIN;
        let quarter = (MARGIN + chart_w * 0.25) as i32;
        let three_quarters = (MARGIN + chart_w * 0.75) as i32;
        let tint = |z: (f64, f64, f64)| {
            let c = |zc: f64, bg: f64| ((zc * 0.65 + bg * 0.35) * 255.0).round() as i32;
            (c(z.0, BG.0), c(z.1, BG.1), c(z.2, BG.2))
        };
        let close = |a: (u8, u8, u8), b: (i32, i32, i32)| {
            (a.0 as i32 - b.0).abs() <= 2
                && (a.1 as i32 - b.1).abs() <= 2
                && (a.2 as i32 - b.2).abs() <= 2
        };
        assert!(close(
            pixel(&mut surface, quarter, 640),
            tint(ZONE_COLORS[1])
        ));
        assert!(close(
            pixel(&mut surface, three_quarters, 640),
            tint(ZONE_COLORS[3])
        ));
        // Z2 is lower than Z4: above the first block is still background.
        assert_eq!(brightness(pixel(&mut surface, quarter, 400)), BG_BRIGHTNESS);
        // And no block runs on below the chart.
        assert_eq!(brightness(pixel(&mut surface, quarter, 680)), BG_BRIGHTNESS);
        // Nor past its right-hand edge into the margin.
        assert_eq!(
            brightness(pixel(&mut surface, CARD_WIDTH - 32, 640)),
            BG_BRIGHTNESS
        );
    }

    #[test]
    fn should_stretch_the_scale_rather_than_cut_off_a_sprint() {
        // 600 W against FTP 250 is far over the 312 W top of the usual scale.
        // Stretched to 630 W, it is drawn at 660 − 420 × 600/630 = 260 px;
        // clipped, it would be a flat line along the top at 240.
        let session = route_hour(vec![Some(600); 3600], 250);
        let mut surface = render(&session, "Sprint", 250).unwrap();
        let x = CARD_WIDTH / 2;
        assert!(brightness(pixel(&mut surface, x, 260)) > 600);
        assert_eq!(brightness(pixel(&mut surface, x, 245)), BG_BRIGHTNESS);
    }

    #[test]
    fn should_leave_a_gap_where_no_power_was_recorded() {
        // Power for the first and last 20 minutes only: the middle of the
        // chart must be empty, not bridged by the line or its fill.
        let watts = (0..3600)
            .map(|i| (!(1200..2400).contains(&i)).then_some(200))
            .collect();
        let session = route_hour(watts, 250);
        let mut surface = render(&session, "Gap", 250).unwrap();
        for y in [391, 500, 650] {
            assert_eq!(
                brightness(pixel(&mut surface, CARD_WIDTH / 2, y)),
                BG_BRIGHTNESS,
                "y = {y}"
            );
        }
        assert!(brightness(pixel(&mut surface, 200, 500)) > BG_BRIGHTNESS);
        assert!(brightness(pixel(&mut surface, 1400, 500)) > BG_BRIGHTNESS);
    }

    #[test]
    fn should_mark_ftp_with_a_dashed_line() {
        // 100 W rides well below it. FTP 250 of 312.5 is at 660 − 336 = 324 px;
        // the first dash runs from the chart's left edge.
        let session = route_hour(vec![Some(100); 3600], 250);
        let mut surface = render(&session, "Easy", 250).unwrap();
        assert!(
            brightness(pixel(&mut surface, 68, 324)) > BG_BRIGHTNESS + 60,
            "on a dash"
        );
        assert_eq!(
            brightness(pixel(&mut surface, 76, 324)),
            BG_BRIGHTNESS,
            "between dashes"
        );
    }

    #[test]
    fn should_render_a_ride_with_no_data_without_failing() {
        let session = Session::new(None);
        assert!(render(&session, "", 250).is_ok());
    }

    #[test]
    fn should_ellipsize_a_title_too_long_for_the_card() {
        let surface = cairo::ImageSurface::create(cairo::Format::Rgb24, 10, 10).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        cr.set_font_size(56.0);
        let long = "Çok uzun bir antrenman adı ".repeat(10);
        let fitted = fit_text(&cr, &long, 600.0).unwrap();
        assert!(fitted.ends_with('…'));
        assert!(cr.text_extents(&fitted).unwrap().x_advance() <= 600.0);
        assert_eq!(fit_text(&cr, "Short", 600.0).unwrap(), "Short");
    }
}
