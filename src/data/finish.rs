//! The finish: a red arch over the road where the route ends, a banner with
//! FINISH on it, and a chequered line across the track below.
//!
//! Scenery only (rule 5): it stands where the route's own road ends, and
//! nothing about it reaches the ride. Built from boxes, in the world's own
//! low-poly style; the letters are blocks too, set out on a five-by-seven
//! grid.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::animals::cuboid;
use crate::data::course3d::{Mesh, RoadPoint, MATERIAL_PLAIN};
use crate::data::palette;
use crate::data::roadway::TRACK_HALF_WIDTH_M;

/// The posts stand this far either side of the road's line: on the grass,
/// clear of the track.
const POST_AT_M: f32 = TRACK_HALF_WIDTH_M + 0.9;
const POST_W_M: f32 = 0.6;
/// The top of the crossbar, above the road.
const TOP_M: f32 = 5.6;
const BAR_H_M: f32 = 0.6;
/// The banner hangs under the crossbar, across the track.
const BANNER_HALF_W_M: f32 = TRACK_HALF_WIDTH_M;
const BANNER_LOW_M: f32 = 3.5;
const BANNER_DEPTH_M: f32 = 0.12;
/// A chequered strip along the banner's top, squares this size.
const STRIP_M: f32 = 0.3;
/// The letters' grid squares, and how far they stand off the banner: the
/// depth buffer tells 4 cm apart from the banner to well past the haze.
const PIXEL_M: f32 = 0.13;
const PROUD_M: f32 = 0.04;
/// The chequered line across the road: two rows of squares, sitting above
/// the road's own paint.
const LINE_SQUARE_M: f32 = TRACK_HALF_WIDTH_M / 5.0;
const LINE_LIFT_M: f32 = 0.04;
/// The posts go this far into the ground, so a bank beside the road never
/// leaves one standing on air.
const SINK_M: f32 = 1.5;

/// FINISH on a five-by-seven grid, top row first.
const LETTERS: [[&str; 7]; 6] = [
    [
        "11111", "10000", "10000", "11110", "10000", "10000", "10000",
    ],
    [
        "01110", "00100", "00100", "00100", "00100", "00100", "01110",
    ],
    [
        "10001", "11001", "10101", "10011", "10001", "10001", "10001",
    ],
    [
        "01110", "00100", "00100", "00100", "00100", "00100", "01110",
    ],
    [
        "01111", "10000", "10000", "01110", "00001", "00001", "11110",
    ],
    [
        "10001", "10001", "10001", "11111", "10001", "10001", "10001",
    ],
];

/// The arch over the end of `road`, the route's own road; empty for a road
/// too short to have a direction.
pub fn arch(road: &[RoadPoint]) -> Mesh {
    let mut mesh = Mesh::default();
    let [.., before, end] = road else {
        return mesh;
    };
    let (dx, dz) = (end.pos[0] - before.pos[0], end.pos[2] - before.pos[2]);
    let len = dx.hypot(dz);
    if len < 1e-3 {
        return mesh;
    }
    let (fx, fz) = (dx / len, dz / len);
    let (rx, rz) = (-fz, fx);
    let p = &palette::BASE;
    let c = |rgb: [f32; 3]| [rgb[0], rgb[1], rgb[2], MATERIAL_PLAIN];
    // Built across (u, right), up (v) and along (w, forward) the road, then
    // set down at its end.
    let mut add = |lo: [f32; 3], hi: [f32; 3], colour: [f32; 4]| {
        let mut part = cuboid(lo, hi, colour);
        for v in &mut part.vertices {
            let [u, up, w] = v.pos;
            v.pos = [
                end.pos[0] + u * rx + w * fx,
                end.pos[1] + up,
                end.pos[2] + u * rz + w * fz,
            ];
        }
        mesh.extend(part);
    };

    let half_post = POST_W_M / 2.0;
    for side in [-1.0, 1.0] {
        let u = side * POST_AT_M;
        add(
            [u - half_post, -SINK_M, -half_post],
            [u + half_post, TOP_M, half_post],
            c(p.arch),
        );
    }
    add(
        [-POST_AT_M - half_post, TOP_M - BAR_H_M, -half_post],
        [POST_AT_M + half_post, TOP_M, half_post],
        c(p.arch),
    );

    // The banner, its chequered strip, and the letters, all facing the
    // rider on their way in (−w).
    let face = -BANNER_DEPTH_M / 2.0;
    let banner_top = TOP_M - BAR_H_M;
    add(
        [-BANNER_HALF_W_M, BANNER_LOW_M, face],
        [BANNER_HALF_W_M, banner_top, -face],
        c(p.banner),
    );
    let squares = (2.0 * BANNER_HALF_W_M / STRIP_M).floor() as i32;
    let strip_from = -(squares as f32) * STRIP_M / 2.0;
    for k in (0..squares).step_by(2) {
        let u = strip_from + k as f32 * STRIP_M;
        add(
            [u, banner_top - STRIP_M, face - PROUD_M],
            [u + STRIP_M, banner_top, -face + PROUD_M],
            c(p.chequer),
        );
    }
    let letter_w = 5.0 * PIXEL_M;
    let word_w = LETTERS.len() as f32 * letter_w + (LETTERS.len() - 1) as f32 * PIXEL_M;
    let text_top = banner_top - STRIP_M - PIXEL_M;
    for (n, rows) in LETTERS.iter().enumerate() {
        let left = -word_w / 2.0 + n as f32 * (letter_w + PIXEL_M);
        for (r, row) in rows.iter().enumerate() {
            let top = text_top - r as f32 * PIXEL_M;
            // Each run of lit squares along a row is one block.
            let bits: Vec<bool> = row.chars().map(|ch| ch == '1').collect();
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
                add(
                    [left + start as f32 * PIXEL_M, top - PIXEL_M, face - PROUD_M],
                    [left + k as f32 * PIXEL_M, top, face],
                    c(p.chequer),
                );
            }
        }
    }

    // The line across the road: dark squares on light, two rows.
    for row in 0..2 {
        let w0 = (row as f32 - 1.0) * LINE_SQUARE_M;
        let mut u = -TRACK_HALF_WIDTH_M;
        let mut k = row;
        while u < TRACK_HALF_WIDTH_M - 1e-4 {
            let colour = if k % 2 == 0 { p.chequer } else { p.banner };
            add(
                [u, 0.0, w0],
                [u + LINE_SQUARE_M, LINE_LIFT_M, w0 + LINE_SQUARE_M],
                c(colour),
            );
            u += LINE_SQUARE_M;
            k += 1;
        }
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight road north, 100 m long, climbing 5 m.
    fn road() -> Vec<RoadPoint> {
        (0..=10)
            .map(|k| RoadPoint {
                pos: [0.0, 0.5 * k as f32, -10.0 * k as f32],
                distance_m: 10.0 * k as f32,
            })
            .collect()
    }

    #[test]
    fn should_stand_the_arch_over_the_end_of_the_road() {
        let m = arch(&road());
        assert!(!m.vertices.is_empty());
        let end = [0.0, 5.0, -100.0];
        for v in &m.vertices {
            // Within its own width across, and a post's depth along.
            assert!((v.pos[0] - end[0]).abs() <= POST_AT_M + POST_W_M / 2.0 + 1e-4);
            assert!((v.pos[2] - end[2]).abs() <= LINE_SQUARE_M + 1e-4);
            assert!(v.pos[1] >= end[1] - SINK_M - 1e-4 && v.pos[1] <= end[1] + TOP_M + 1e-4);
        }
    }

    #[test]
    fn should_leave_the_track_clear_to_ride_under() {
        // Over the track, nothing between the paint on the road and the
        // banner's foot: a rider's head and a camera pass through.
        let m = arch(&road());
        for v in &m.vertices {
            let over_track = v.pos[0].abs() < TRACK_HALF_WIDTH_M - 1e-4;
            let up = v.pos[1] - 5.0;
            if over_track {
                assert!(
                    up <= LINE_LIFT_M + 1e-4 || up >= BANNER_LOW_M - 1e-4,
                    "something {up} m up over the track at {:?}",
                    v.pos
                );
            }
        }
    }

    #[test]
    fn should_paint_the_line_right_across_the_track_above_its_paint() {
        let m = arch(&road());
        let line: Vec<_> = m
            .vertices
            .iter()
            .filter(|v| {
                v.pos[1] - 5.0 <= LINE_LIFT_M + 1e-4 && v.pos[0].abs() <= TRACK_HALF_WIDTH_M + 1e-4
            })
            .collect();
        let lo = line.iter().map(|v| v.pos[0]).fold(f32::MAX, f32::min);
        let hi = line.iter().map(|v| v.pos[0]).fold(f32::MIN, f32::max);
        assert!((lo + TRACK_HALF_WIDTH_M).abs() < 1e-3, "starts at {lo}");
        assert!((hi - TRACK_HALF_WIDTH_M).abs() < 1e-3, "ends at {hi}");
        let top = line.iter().map(|v| v.pos[1] - 5.0).fold(f32::MIN, f32::max);
        assert!(
            top > 0.02 + 0.01,
            "the line's top {top} m is lost in the road's paint"
        );
    }

    #[test]
    fn should_stand_the_letters_a_depth_step_clear_of_the_banner() {
        let m = arch(&road());
        let p = &palette::BASE;
        let banner_face = 100.0 - BANNER_DEPTH_M / 2.0;
        let letters: Vec<_> = m
            .vertices
            .iter()
            .filter(|v| v.colour[..3] == p.chequer && v.pos[1] - 5.0 > BANNER_LOW_M)
            .collect();
        assert!(letters.len() > 100, "{} letter corners", letters.len());
        // The rider comes from the south (+z); the banner's near face is at
        // z = −100 + depth/2, and the letters stand out of it towards them.
        let nearest = letters.iter().map(|v| v.pos[2]).fold(f32::MIN, f32::max);
        assert!(
            nearest - (-banner_face) >= 0.03,
            "letters stand {} m off the banner",
            nearest - (-banner_face)
        );
    }

    #[test]
    fn should_build_nothing_for_a_road_with_no_direction() {
        let one = vec![RoadPoint {
            pos: [0.0; 3],
            distance_m: 0.0,
        }];
        assert!(arch(&one).vertices.is_empty());
        let still = vec![one[0], one[0]];
        assert!(arch(&still).vertices.is_empty());
    }
}
