//! Workout cues in the world: where an interval starts on a workout's road,
//! a sign says what it is — REP 2/5, EASY, COOL-DOWN — in the interval's zone
//! colour, with a stripe of that colour across the road. Seen from a few
//! hundred metres off, a sign is also the next interval coming.
//!
//! Scenery only (rule 5), like the finish arch: built from boxes in the
//! world's low-poly style, the letters blocks on a five-by-seven grid. An
//! effort gets a gantry over the road, so the work that matters stands out;
//! a rest gets a quieter board on the verge (the rider's choice, from
//! mockups of both).
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::animals::cuboid;
use crate::data::athlete::ZONE_COLORS;
use crate::data::course3d::{direction_at, position_at, Mesh, RoadPoint, MATERIAL_PLAIN};
use crate::data::palette;
use crate::data::roadway::TRACK_HALF_WIDTH_M;

/// One sign: `label` at `distance_m` along the road, in power zone `zone`
/// (0 = Z1 … 6 = Z7). An `effort` stands as a gantry, anything else as a
/// board.
#[derive(Debug, Clone, PartialEq)]
pub struct Sign {
    pub distance_m: f32,
    pub label: String,
    pub zone: usize,
    pub effort: bool,
}

/// How a sign stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Over the road on two posts, the label on a banner across it.
    Gantry,
    /// On the verge to the rider's right, a board on two posts.
    Board,
}

// ── The gantry: the finish arch's frame ──────────────────────────────────────
/// Wider than the finish arch: the label must read from far off, and it
/// is the banner's width that sets the letters' size.
const POST_AT_M: f32 = TRACK_HALF_WIDTH_M + 2.6;
const POST_W_M: f32 = 0.4;
const GANTRY_TOP_M: f32 = 6.0;
const BAR_H_M: f32 = 0.4;
const GANTRY_BANNER_LOW_M: f32 = 3.8;

// ── The board on the verge ───────────────────────────────────────────────────
/// The board's middle, across from the road's line: clear of the track
/// and the verge's posts.
const BOARD_AT_M: f32 = TRACK_HALF_WIDTH_M + 3.4;
const BOARD_HALF_W_M: f32 = 2.8;
const BOARD_LOW_M: f32 = 1.4;
const BOARD_HIGH_M: f32 = 3.6;

/// Every banner and board, and the letters on it.
const PANEL_DEPTH_M: f32 = 0.12;
/// Margin round the label on its panel.
const PANEL_MARGIN_M: f32 = 0.2;
/// The largest a letter's grid square may be; long labels get smaller ones.
const MAX_PIXEL_M: f32 = 0.26;
/// As on the finish banner: far enough off the panel for the depth buffer.
const PROUD_M: f32 = 0.04;
/// The stripe across the road where the interval starts.
const STRIPE_DEPTH_M: f32 = 0.6;
const STRIPE_LIFT_M: f32 = 0.04;
/// Posts go into the ground, so a bank never leaves one on air.
const SINK_M: f32 = 1.5;

/// The meshes for `signs` along `road`, a workout's laid road: a gantry
/// for each effort, a board for the rest.
pub fn signs(road: &[RoadPoint], signs: &[Sign]) -> Mesh {
    let mut mesh = Mesh::default();
    for s in signs {
        let look = if s.effort { Look::Gantry } else { Look::Board };
        mesh.extend(sign(road, s, look));
    }
    mesh
}

/// One sign. Empty for a road with no length or a sign off its end.
pub fn sign(road: &[RoadPoint], s: &Sign, look: Look) -> Mesh {
    let mut mesh = Mesh::default();
    let end = road.last().map_or(0.0, |p| p.distance_m);
    if !(s.distance_m > 0.0 && s.distance_m < end) {
        return mesh;
    }
    let (Some(at), Some(dir)) = (
        position_at(road, s.distance_m),
        direction_at(road, s.distance_m),
    ) else {
        return mesh;
    };
    let (fx, fz) = (dir[0], dir[2]);
    let (rx, rz) = (-fz, fx);
    let p = &palette::BASE;
    let c = |rgb: [f32; 3]| [rgb[0], rgb[1], rgb[2], MATERIAL_PLAIN];
    let zone = zone_rgb(s.zone);
    let ink = if luminance(zone) < 0.45 {
        p.banner
    } else {
        p.chequer
    };
    // Across (u, right), up and along (w, forward) the road at the sign.
    let mut add = |lo: [f32; 3], hi: [f32; 3], colour: [f32; 4]| {
        let mut part = cuboid(lo, hi, colour);
        for v in &mut part.vertices {
            let [u, up, w] = v.pos;
            v.pos = [at[0] + u * rx + w * fx, at[1] + up, at[2] + u * rz + w * fz];
        }
        mesh.extend(part);
    };

    // The panel's centre across, its half width, its foot and its top.
    let (mid, half_w, low, high) = match look {
        Look::Gantry => {
            let half = POST_W_M / 2.0;
            for side in [-1.0, 1.0] {
                let u = side * POST_AT_M;
                add(
                    [u - half, -SINK_M, -half],
                    [u + half, GANTRY_TOP_M, half],
                    c(p.concrete),
                );
            }
            add(
                [-POST_AT_M - half, GANTRY_TOP_M - BAR_H_M, -half],
                [POST_AT_M + half, GANTRY_TOP_M, half],
                c(p.concrete),
            );
            (
                0.0,
                POST_AT_M - half,
                GANTRY_BANNER_LOW_M,
                GANTRY_TOP_M - BAR_H_M,
            )
        }
        Look::Board => {
            let half = 0.08;
            for side in [-1.0, 1.0] {
                let u = BOARD_AT_M + side * (BOARD_HALF_W_M - 0.4);
                add(
                    [u - half, -SINK_M, -half],
                    [u + half, BOARD_LOW_M, half],
                    c(p.concrete),
                );
            }
            (BOARD_AT_M, BOARD_HALF_W_M, BOARD_LOW_M, BOARD_HIGH_M)
        }
    };
    let face = -PANEL_DEPTH_M / 2.0;
    add(
        [mid - half_w, low, face],
        [mid + half_w, high, -face],
        c(zone),
    );
    for (lo, hi) in text_blocks(
        &s.label,
        2.0 * (half_w - PANEL_MARGIN_M),
        high - low - 2.0 * PANEL_MARGIN_M,
    ) {
        let centre_up = (low + high) / 2.0;
        add(
            [mid + lo[0], centre_up + lo[1], face - PROUD_M],
            [mid + hi[0], centre_up + hi[1], face],
            c(ink),
        );
    }

    // The stripe across the track.
    add(
        [-TRACK_HALF_WIDTH_M, 0.0, -STRIPE_DEPTH_M / 2.0],
        [TRACK_HALF_WIDTH_M, STRIPE_LIFT_M, STRIPE_DEPTH_M / 2.0],
        c(zone),
    );
    mesh
}

/// The zone's colour, as the app's zone bars show it. Out of range is Z7.
fn zone_rgb(zone: usize) -> [f32; 3] {
    let (r, g, b) = ZONE_COLORS[zone.min(ZONE_COLORS.len() - 1)];
    [r as f32, g as f32, b as f32]
}

fn luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// The blocks that spell `label`, centred on (0, 0) and fitted into `width`
/// by `height`: each block's (across, up) corners. A character with no glyph
/// is a gap.
pub fn text_blocks(label: &str, width: f32, height: f32) -> Vec<([f32; 2], [f32; 2])> {
    let chars: Vec<char> = label.chars().collect();
    // Written so a NaN size, which compares false with everything, draws nothing.
    if chars.is_empty() || !(width > 0.0 && height > 0.0) {
        return Vec::new();
    }
    // Five squares a letter and one between: 6n − 1 across, 7 up.
    let cols = 6 * chars.len() - 1;
    let pixel = MAX_PIXEL_M.min(width / cols as f32).min(height / 7.0);
    let left = -(cols as f32) * pixel / 2.0;
    let top = 3.5 * pixel;
    let mut blocks = Vec::new();
    for (n, ch) in chars.iter().enumerate() {
        let Some(rows) = glyph(*ch) else {
            continue;
        };
        let x0 = left + (6 * n) as f32 * pixel;
        for (r, row) in rows.iter().enumerate() {
            let y1 = top - r as f32 * pixel;
            let bits: Vec<bool> = row.chars().map(|b| b == '1').collect();
            // Each run of lit squares along a row is one block.
            let mut k = 0;
            while k < bits.len() {
                if !bits[k] {
                    k += 1;
                    continue;
                }
                let start = k;
                while k < bits.len() && bits[k] {
                    k += 1;
                }
                blocks.push((
                    [x0 + start as f32 * pixel, y1 - pixel],
                    [x0 + k as f32 * pixel, y1],
                ));
            }
        }
    }
    blocks
}

/// A character on the five-by-seven grid, top row first. Lower case reads as
/// upper; anything else with no glyph is `None`.
fn glyph(ch: char) -> Option<[&'static str; 7]> {
    Some(match ch.to_ascii_uppercase() {
        'A' => [
            "01110", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'B' => [
            "11110", "10001", "10001", "11110", "10001", "10001", "11110",
        ],
        'C' => [
            "01110", "10001", "10000", "10000", "10000", "10001", "01110",
        ],
        'D' => [
            "11110", "10001", "10001", "10001", "10001", "10001", "11110",
        ],
        'E' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        'F' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "10000",
        ],
        'G' => [
            "01110", "10001", "10000", "10111", "10001", "10001", "01111",
        ],
        'H' => [
            "10001", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'I' => [
            "01110", "00100", "00100", "00100", "00100", "00100", "01110",
        ],
        'J' => [
            "00111", "00010", "00010", "00010", "00010", "10010", "01100",
        ],
        'K' => [
            "10001", "10010", "10100", "11000", "10100", "10010", "10001",
        ],
        'L' => [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        'M' => [
            "10001", "11011", "10101", "10101", "10001", "10001", "10001",
        ],
        'N' => [
            "10001", "11001", "10101", "10011", "10001", "10001", "10001",
        ],
        'O' => [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'P' => [
            "11110", "10001", "10001", "11110", "10000", "10000", "10000",
        ],
        'Q' => [
            "01110", "10001", "10001", "10001", "10101", "10010", "01101",
        ],
        'R' => [
            "11110", "10001", "10001", "11110", "10100", "10010", "10001",
        ],
        'S' => [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
        'T' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        'U' => [
            "10001", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'V' => [
            "10001", "10001", "10001", "10001", "10001", "01010", "00100",
        ],
        'W' => [
            "10001", "10001", "10001", "10101", "10101", "10101", "01010",
        ],
        'X' => [
            "10001", "10001", "01010", "00100", "01010", "10001", "10001",
        ],
        'Y' => [
            "10001", "10001", "01010", "00100", "00100", "00100", "00100",
        ],
        'Z' => [
            "11111", "00001", "00010", "00100", "01000", "10000", "11111",
        ],
        '0' => [
            "01110", "10001", "10011", "10101", "11001", "10001", "01110",
        ],
        '1' => [
            "00100", "01100", "00100", "00100", "00100", "00100", "01110",
        ],
        '2' => [
            "01110", "10001", "00001", "00010", "00100", "01000", "11111",
        ],
        '3' => [
            "11111", "00010", "00100", "00010", "00001", "10001", "01110",
        ],
        '4' => [
            "00010", "00110", "01010", "10010", "11111", "00010", "00010",
        ],
        '5' => [
            "11111", "10000", "11110", "00001", "00001", "10001", "01110",
        ],
        '6' => [
            "00110", "01000", "10000", "11110", "10001", "10001", "01110",
        ],
        '7' => [
            "11111", "00001", "00010", "00100", "01000", "01000", "01000",
        ],
        '8' => [
            "01110", "10001", "10001", "01110", "10001", "10001", "01110",
        ],
        '9' => [
            "01110", "10001", "10001", "01111", "00001", "00010", "01100",
        ],
        '/' => [
            "00001", "00010", "00010", "00100", "01000", "01000", "10000",
        ],
        '-' => [
            "00000", "00000", "00000", "11111", "00000", "00000", "00000",
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight road north, 400 m long, climbing 20 m.
    fn road() -> Vec<RoadPoint> {
        (0..=40)
            .map(|k| RoadPoint {
                pos: [0.0, 0.5 * k as f32, -10.0 * k as f32],
                distance_m: 10.0 * k as f32,
            })
            .collect()
    }

    fn sign_at(distance_m: f32, label: &str, zone: usize) -> Sign {
        Sign {
            distance_m,
            label: label.into(),
            zone,
            effort: true,
        }
    }

    /// Height above the road at 200 m, where the test signs stand.
    const AT_Y: f32 = 10.0;

    #[test]
    fn should_fit_every_label_inside_its_panel() {
        for label in [
            "GO",
            "EASY",
            "REP 2/4",
            "REP 12/16",
            "LAST REP",
            "COOL-DOWN",
        ] {
            for (w, h) in [(9.6, 1.4), (5.2, 1.8), (1.0, 0.3)] {
                let blocks = text_blocks(label, w, h);
                assert!(!blocks.is_empty(), "{label} drew nothing");
                for (lo, hi) in &blocks {
                    assert!(
                        lo[0] >= -w / 2.0 - 1e-4 && hi[0] <= w / 2.0 + 1e-4,
                        "{label}"
                    );
                    assert!(
                        lo[1] >= -h / 2.0 - 1e-4 && hi[1] <= h / 2.0 + 1e-4,
                        "{label}"
                    );
                    assert!(lo[0] < hi[0] && lo[1] < hi[1], "{label}: an empty block");
                }
            }
        }
    }

    #[test]
    fn should_have_a_glyph_for_every_character_a_sign_can_say() {
        for ch in "WARM-UP EASY COOL-DOWN LAST REP GO 0123456789/".chars() {
            assert!(ch == ' ' || glyph(ch).is_some(), "no glyph for {ch:?}");
        }
    }

    #[test]
    fn should_draw_nothing_for_an_empty_label_or_no_room() {
        assert!(text_blocks("", 5.0, 1.0).is_empty());
        assert!(text_blocks("GO", 0.0, 1.0).is_empty());
        assert!(text_blocks("GO", 5.0, -1.0).is_empty());
        assert!(text_blocks("GO", f32::NAN, 1.0).is_empty());
    }

    #[test]
    fn should_keep_a_gap_for_a_character_with_no_glyph() {
        // The ? has no glyph: the second A stays where it would be.
        let with = text_blocks("A?A", 9.0, 2.0);
        let plain = text_blocks("AAA", 9.0, 2.0);
        let right =
            |b: &[([f32; 2], [f32; 2])]| b.iter().map(|(_, hi)| hi[0]).fold(f32::MIN, f32::max);
        assert_eq!(right(&with), right(&plain));
        assert_eq!(text_blocks("easy", 5.0, 1.0), text_blocks("EASY", 5.0, 1.0));
    }

    #[test]
    fn should_leave_the_track_clear_to_ride_under_a_gantry() {
        let m = sign(&road(), &sign_at(200.0, "REP 2/4", 3), Look::Gantry);
        assert!(!m.vertices.is_empty());
        for v in &m.vertices {
            let up = v.pos[1] - AT_Y;
            if v.pos[0].abs() < TRACK_HALF_WIDTH_M - 1e-4 {
                assert!(
                    up <= STRIPE_LIFT_M + 1e-4 || up >= GANTRY_BANNER_LOW_M - 1e-4,
                    "something {up} m up over the track at {:?}",
                    v.pos
                );
            }
        }
    }

    #[test]
    fn should_keep_a_board_off_the_track_on_the_right() {
        let m = sign(&road(), &sign_at(200.0, "EASY", 0), Look::Board);
        assert!(!m.vertices.is_empty());
        for v in &m.vertices {
            let up = v.pos[1] - AT_Y;
            // Only the stripe is on the track; the rest stands on the
            // rider's right — east, riding north.
            if !(-1e-4..=STRIPE_LIFT_M + 1e-4).contains(&up) {
                assert!(
                    v.pos[0] >= TRACK_HALF_WIDTH_M,
                    "board part over the track at {:?}",
                    v.pos
                );
            }
        }
    }

    #[test]
    fn should_paint_the_stripe_right_across_the_track_where_the_interval_starts() {
        let m = sign(&road(), &sign_at(200.0, "EASY", 0), Look::Board);
        let stripe: Vec<_> = m
            .vertices
            .iter()
            .filter(|v| v.pos[1] - AT_Y <= STRIPE_LIFT_M + 1e-4 && v.pos[1] >= AT_Y - 1e-4)
            .collect();
        let lo = stripe.iter().map(|v| v.pos[0]).fold(f32::MAX, f32::min);
        let hi = stripe.iter().map(|v| v.pos[0]).fold(f32::MIN, f32::max);
        assert_eq!((lo, hi), (-TRACK_HALF_WIDTH_M, TRACK_HALF_WIDTH_M));
        for v in &stripe {
            assert!(
                (v.pos[2] + 200.0).abs() <= STRIPE_DEPTH_M / 2.0 + 1e-3,
                "at z {}",
                v.pos[2]
            );
        }
    }

    #[test]
    fn should_stand_no_sign_off_the_road_or_at_its_ends() {
        for d in [0.0, -5.0, 400.0, 401.0, f32::NAN, f32::INFINITY] {
            let m = sign(&road(), &sign_at(d, "GO", 3), Look::Gantry);
            assert!(m.vertices.is_empty(), "a sign at {d} m");
        }
        assert!(sign(&[], &sign_at(10.0, "GO", 3), Look::Board)
            .vertices
            .is_empty());
        // Just inside either end still stands.
        assert!(!sign(&road(), &sign_at(0.5, "GO", 3), Look::Gantry)
            .vertices
            .is_empty());
        assert!(!sign(&road(), &sign_at(399.5, "GO", 3), Look::Gantry)
            .vertices
            .is_empty());
    }

    #[test]
    fn should_read_a_zone_past_z7_as_z7() {
        assert_eq!(zone_rgb(99), zone_rgb(6));
        assert!(!sign(&road(), &sign_at(200.0, "GO", 99), Look::Gantry)
            .vertices
            .is_empty());
    }

    #[test]
    fn should_letter_dark_zones_in_white_and_bright_ones_in_ink() {
        let p = &palette::BASE;
        let ink_of = |zone: usize| {
            let m = sign(&road(), &sign_at(200.0, "GO", zone), Look::Board);
            let lettered = |c: [f32; 3]| m.vertices.iter().any(|v| v.colour[..3] == c);
            (lettered(p.banner), lettered(p.chequer))
        };
        for zone in [5, 6] {
            assert_eq!(ink_of(zone), (true, false), "Z{}", zone + 1);
        }
        for zone in [0, 1, 2, 3, 4] {
            assert_eq!(ink_of(zone), (false, true), "Z{}", zone + 1);
        }
    }

    #[test]
    fn should_stand_a_gantry_for_an_effort_and_a_board_for_a_rest() {
        let r = road();
        let rest = Sign {
            effort: false,
            ..sign_at(200.0, "EASY", 0)
        };
        let effort = sign_at(200.0, "EASY", 0);
        assert_eq!(
            signs(&r, std::slice::from_ref(&rest)),
            sign(&r, &rest, Look::Board)
        );
        assert_eq!(
            signs(&r, std::slice::from_ref(&effort)),
            sign(&r, &effort, Look::Gantry)
        );
    }
}
