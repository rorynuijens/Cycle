//! The ridden road: a game track along the made-up road.
//!
//! One width everywhere, centred on the road's line, yellow edges and a
//! dashed centre line (the stylized-route skill, §5). The road never comes
//! back on itself (`data/track.rs`), so it needs no bridges and is never
//! drawn twice over one place.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::course3d::{Mesh, RoadPoint, Vertex, MATERIAL_PLAIN, MATERIAL_ROAD};
use crate::data::palette;

/// A dashed line: painted this long in every period.
const DASH_ON_M: f32 = 3.0;
const DASH_PERIOD_M: f32 = 9.0;

/// Lift of paint above the road, in metres.
const PAINT_LIFT_M: f32 = 0.02;

/// The track's half width, the same everywhere: a two-lane road, the rider
/// in the right-hand lane.
pub const TRACK_HALF_WIDTH_M: f32 = 2.6;
/// The yellow edge stripes: this wide, this far in from the edge.
const STRIPE_M: f32 = 0.25;
const STRIPE_INSET_M: f32 = 0.1;
/// The dashed centre line's half width.
const CENTRE_HALF_WIDTH_M: f32 = 0.08;
/// Grass banks hang this far down from either edge of the road, so where
/// the ground drops away from it — round a steep hairpin, or under a jump in
/// a GPS's heights — the road stands on an embankment instead of floating.
pub const BANK_M: f32 = 20.0;

/// Colours, from the shared palette: the track looks the same in every biome.
fn rgba(rgb: [f32; 3], material: f32) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], material]
}

/// Unit direction of travel at each point of the line, flat.
fn headings(line: &[RoadPoint]) -> Vec<(f32, f32)> {
    let n = line.len();
    (0..n)
        .map(|i| {
            let a = line[i.saturating_sub(1)].pos;
            let b = line[(i + 1).min(n - 1)].pos;
            let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
            let len = (dx * dx + dz * dz).sqrt();
            if len > 1e-3 {
                (dx / len, dz / len)
            } else {
                (0.0, -1.0)
            }
        })
        .collect()
}

/// Perpendicular to the line at each point, pointing right.
fn rights(line: &[RoadPoint]) -> Vec<(f32, f32)> {
    headings(line)
        .into_iter()
        .map(|(hx, hz)| (-hz, hx))
        .collect()
}

/// A strip along the line between offsets `from` and `to` (right positive),
/// at `lift` above the line.
fn strip(line: &[RoadPoint], from: f32, to: f32, lift: f32, colour: [f32; 4]) -> Mesh {
    let right = rights(line);
    let pair = |i: usize| {
        let (p, (rx, rz)) = (line[i].pos, right[i]);
        let y = p[1] + lift;
        let at = |off: f32| [p[0] + rx * off, y, p[2] + rz * off];
        [at(from), at(to)]
    };
    quads_along(line.len(), pair, colour)
}

/// Quads joining pairs of points along a line — each point gives two corners
/// shared by the quads either side of it.
fn quads_along(points: usize, pair: impl Fn(usize) -> [[f32; 3]; 2], colour: [f32; 4]) -> Mesh {
    let mut mesh = Mesh::default();
    for i in 0..points {
        for pos in pair(i) {
            mesh.vertices.push(Vertex { pos, colour });
        }
        if i > 0 {
            let (a, b) = (2 * i as u32 - 2, 2 * i as u32);
            mesh.indices
                .extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    mesh
}

/// Dashes along the line at `offset` (right positive), each [`DASH_ON_M`]
/// long every [`DASH_PERIOD_M`], placed by distance along the route and cut
/// to exact length — the road's points are up to 6 m apart, near the
/// pattern's length, so dashes cut from its segments would run together.
fn dashes(line: &[RoadPoint], offset: f32, half_width: f32, colour: [f32; 4]) -> Mesh {
    let mut mesh = Mesh::default();
    let (Some(first), Some(last)) = (line.first(), line.last()) else {
        return mesh;
    };
    if line.len() < 2 {
        return mesh;
    }
    let right = rights(line);
    // Point, right vector and offset at a distance along the line, with the
    // index of the segment it falls in.
    let at = |d: f32| {
        let i = line
            .partition_point(|p| p.distance_m <= d)
            .clamp(1, line.len() - 1);
        let (a, b) = (&line[i - 1], &line[i]);
        let span = b.distance_m - a.distance_m;
        let t = if span > 1e-3 {
            ((d - a.distance_m) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mix = |u: f32, v: f32| u + (v - u) * t;
        let pos = [
            mix(a.pos[0], b.pos[0]),
            mix(a.pos[1], b.pos[1]),
            mix(a.pos[2], b.pos[2]),
        ];
        let (ra, rb) = (right[i - 1], right[i]);
        let r = (mix(ra.0, rb.0), mix(ra.1, rb.1));
        (pos, r)
    };
    let mut d = (first.distance_m / DASH_PERIOD_M).ceil() * DASH_PERIOD_M;
    while d + DASH_ON_M <= last.distance_m {
        let v = mesh.vertices.len() as u32;
        for (p, r) in [at(d), at(d + DASH_ON_M)] {
            for side in [-half_width, half_width] {
                let off = offset + side;
                mesh.vertices.push(Vertex {
                    pos: [p[0] + r.0 * off, p[1] + PAINT_LIFT_M, p[2] + r.1 * off],
                    colour,
                });
            }
        }
        mesh.indices
            .extend_from_slice(&[v, v + 1, v + 2, v + 1, v + 3, v + 2]);
        d += DASH_PERIOD_M;
    }
    mesh
}

/// The ridden road: the track's surface, a yellow stripe along each edge and
/// a dashed line down the middle.
pub fn ridden_mesh(line: &[RoadPoint]) -> Mesh {
    let p = &palette::BASE;
    let half = TRACK_HALF_WIDTH_M;
    let mut mesh = strip(line, -half, half, 0.0, rgba(p.road, MATERIAL_ROAD));
    let stripe = rgba(p.road_edge, MATERIAL_PLAIN);
    mesh.extend(strip(
        line,
        half - STRIPE_INSET_M - STRIPE_M,
        half - STRIPE_INSET_M,
        PAINT_LIFT_M,
        stripe,
    ));
    mesh.extend(strip(
        line,
        -half + STRIPE_INSET_M,
        -half + STRIPE_INSET_M + STRIPE_M,
        PAINT_LIFT_M,
        stripe,
    ));
    mesh.extend(dashes(
        line,
        0.0,
        CENTRE_HALF_WIDTH_M,
        rgba(p.road_line, MATERIAL_PLAIN),
    ));
    mesh.extend(banks(line, rgba(p.grass_high, MATERIAL_PLAIN)));
    mesh
}

/// A grass face down from each edge of the road, [`BANK_M`] deep.
fn banks(line: &[RoadPoint], colour: [f32; 4]) -> Mesh {
    let right = rights(line);
    let mut mesh = Mesh::default();
    for edge in [-TRACK_HALF_WIDTH_M, TRACK_HALF_WIDTH_M] {
        let pair = |i: usize| {
            let (p, (rx, rz)) = (line[i].pos, right[i]);
            let (x, z) = (p[0] + rx * edge, p[2] + rz * edge);
            [[x, p[1] - BANK_M, z], [x, p[1], z]]
        };
        mesh.extend(quads_along(line.len(), pair, colour));
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_cut_dashes_to_length_whatever_the_point_spacing() {
        // Points 8.5 m apart must still give 3 m dashes every 9 m.
        let line: Vec<RoadPoint> = (0..120)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 8.5],
                distance_m: i as f32 * 8.5,
            })
            .collect();
        let mesh = dashes(&line, -1.5, 0.07, [1.0; 4]);
        let quads = mesh.indices.len() / 6;
        // 0 → 1011.5 m: dashes start at 0, 9, … up to 1008 (ending 1011).
        assert_eq!(quads, 113);
        for q in 0..quads {
            let v = &mesh.vertices[q * 4..q * 4 + 4];
            let len = (v[2].pos[2] - v[0].pos[2]).abs();
            assert!((len - DASH_ON_M).abs() < 1e-3, "dash {q} is {len} m");
            assert!((v[0].pos[0] - (-1.5 - 0.07)).abs() < 1e-4);
        }
    }

    #[test]
    fn should_paint_the_track_with_yellow_edges_and_a_dashed_centre() {
        let line = north_line(200);
        let mesh = ridden_mesh(&line);
        let p = palette::BASE;
        // The line runs north along x = 0, so x is the offset from it.
        let xs = |rgb: [f32; 3]| -> Vec<f32> {
            let mut xs: Vec<f32> = mesh
                .vertices
                .iter()
                .filter(|v| v.colour[..3] == rgb)
                .map(|v| (v.pos[0] * 100.0).round() / 100.0)
                .collect();
            xs.sort_by(f32::total_cmp);
            xs.dedup();
            xs
        };
        assert_eq!(xs(p.road), vec![-2.6, 2.6], "the surface spans the track");
        assert_eq!(xs(p.road_edge), vec![-2.5, -2.25, 2.25, 2.5]);
        assert_eq!(xs(p.road_line), vec![-0.08, 0.08]);
        // Paint on the surface, never under it.
        assert!(mesh
            .vertices
            .iter()
            .filter(|v| v.colour[..3] == p.road_edge || v.colour[..3] == p.road_line)
            .all(|v| v.pos[1] > 10.0));
        // Banks hang from the very edges, down into the ground.
        let banks: Vec<&Vertex> = mesh
            .vertices
            .iter()
            .filter(|v| v.colour[..3] == p.grass_high)
            .collect();
        assert!(!banks.is_empty());
        assert!(banks.iter().all(|v| (v.pos[0].abs() - 2.6).abs() < 1e-4));
        assert!(banks
            .iter()
            .all(|v| v.pos[1] == 10.0 || v.pos[1] == 10.0 - BANK_M));
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
    }

    #[test]
    fn should_keep_the_track_its_full_width_round_a_tight_bend() {
        // A quarter circle of 10 m, a point every 3°: every edge vertex is
        // the half width from the line, square to it.
        let line: Vec<RoadPoint> = (0..=30)
            .map(|k| {
                let a = k as f32 * 3f32.to_radians();
                RoadPoint {
                    pos: [10.0 - 10.0 * a.cos(), 0.0, -10.0 * a.sin()],
                    distance_m: 10.0 * a,
                }
            })
            .collect();
        let surface = strip(
            &line,
            -TRACK_HALF_WIDTH_M,
            TRACK_HALF_WIDTH_M,
            0.0,
            [1.0; 4],
        );
        for (k, v) in surface.vertices.iter().enumerate() {
            let c = line[k / 2].pos;
            let r = (v.pos[0] - c[0]).hypot(v.pos[2] - c[2]);
            assert!((r - TRACK_HALF_WIDTH_M).abs() < 0.02, "{r} at {k}");
        }
    }

    /// A line due north (−z) along x = 0, a point every 8 m.
    fn north_line(points: usize) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 8.0],
                distance_m: i as f32 * 8.0,
            })
            .collect()
    }
}
