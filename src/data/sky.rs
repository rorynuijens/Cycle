//! The far backdrop: mountains on the horizon and clouds in the sky.
//!
//! Both are built around the eye, not the world — the view draws them as if
//! the rider stood at the origin, so they never come nearer however far the
//! ride goes, as the horizon never does. They are drawn behind everything
//! else in the scene.
//!
//! Pure: no GTK, no GL. Eye space: metres, x east, y up from the eye, z south.

use crate::data::animals::cuboid;
use crate::data::course3d::{Mesh, Vertex, MATERIAL_BACKDROP, MATERIAL_CLOUD};
use crate::data::palette;
use crate::data::world::splitmix;

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// How many peaks stand round the horizon.
const PEAKS: u64 = 56;
/// Mountains stand between these distances from the eye.
pub const MOUNTAINS_FROM_M: f32 = 4_400.0;
pub const MOUNTAINS_TO_M: f32 = 5_600.0;
/// No mountain's foot reaches nearer than this: the land ends inside it.
const LAND_EDGE_M: f32 = 3_400.0;
/// Their feet are this far below the eye, under the land's own edge.
const FOOT_M: f32 = -120.0;
/// Peaks this tall carry snow on their top part.
const SNOW_FROM_M: f32 = 700.0;
/// Sides of a mountain: a low-poly pyramid.
const MOUNTAIN_SIDES: usize = 5;

/// The mountains round the horizon, seeded by `seed`: a few ranges of
/// pyramids of different heights, with snow on the tallest.
pub fn mountains(seed: u64) -> Mesh {
    let mut m = Mesh::default();
    let rock = palette::BASE.mountain;
    let snow = palette::BASE.mountain_snow;
    for k in 0..PEAKS {
        let h = splitmix(seed ^ 0x3047 ^ k);
        let azimuth = (k as f32 + unit(h)) / PEAKS as f32 * std::f32::consts::TAU;
        let dist = MOUNTAINS_FROM_M + (MOUNTAINS_TO_M - MOUNTAINS_FROM_M) * unit(h.rotate_left(9));
        // Ranges: some stretches of horizon have high peaks, some low hills.
        let range = (azimuth * 2.0 + unit(seed) * 6.0).sin() * 0.5 + 0.5;
        let height = 180.0 + (250.0 + 900.0 * range) * (0.4 + 0.6 * unit(h.rotate_left(21)));
        let radius = (height * (1.2 + 0.8 * unit(h.rotate_left(33)))).min(dist - LAND_EDGE_M);
        let (cx, cz) = (dist * azimuth.cos(), dist * azimuth.sin());
        let turn = unit(h.rotate_left(45)) * std::f32::consts::TAU;
        let top = FOOT_M + height;
        let ring = |r: f32, y: f32| -> Vec<[f32; 3]> {
            (0..MOUNTAIN_SIDES)
                .map(|s| {
                    let a = turn + s as f32 / MOUNTAIN_SIDES as f32 * std::f32::consts::TAU;
                    [cx + r * a.cos(), y, cz + r * a.sin()]
                })
                .collect()
        };
        let foot = ring(radius, FOOT_M);
        let apex = [cx, top, cz];
        if height > SNOW_FROM_M {
            // The snow line two thirds up: a band of rock, then a snow cap.
            let f = 0.66;
            let line = ring(radius * (1.0 - f), FOOT_M + height * f);
            band(&mut m, &foot, &line, rock);
            cap(&mut m, &line, apex, snow);
        } else {
            cap(&mut m, &foot, apex, rock);
        }
    }
    m
}

fn push(m: &mut Mesh, pos: [f32; 3], rgb: [f32; 3], material: f32) -> u32 {
    m.vertices.push(Vertex {
        pos,
        colour: [rgb[0], rgb[1], rgb[2], material],
    });
    m.vertices.len() as u32 - 1
}

/// Sloping sides from the ring `lo` up to the ring `hi`.
fn band(m: &mut Mesh, lo: &[[f32; 3]], hi: &[[f32; 3]], rgb: [f32; 3]) {
    let n = lo.len();
    for s in 0..n {
        let t = (s + 1) % n;
        let a = push(m, lo[s], rgb, MATERIAL_BACKDROP);
        let b = push(m, lo[t], rgb, MATERIAL_BACKDROP);
        let c = push(m, hi[t], rgb, MATERIAL_BACKDROP);
        let d = push(m, hi[s], rgb, MATERIAL_BACKDROP);
        m.indices.extend_from_slice(&[a, b, c, a, c, d]);
    }
}

/// Sides from the ring up to a point.
fn cap(m: &mut Mesh, ring: &[[f32; 3]], apex: [f32; 3], rgb: [f32; 3]) {
    let n = ring.len();
    for s in 0..n {
        let a = push(m, ring[s], rgb, MATERIAL_BACKDROP);
        let b = push(m, ring[(s + 1) % n], rgb, MATERIAL_BACKDROP);
        let c = push(m, apex, rgb, MATERIAL_BACKDROP);
        m.indices.extend_from_slice(&[a, b, c]);
    }
}

/// How many clouds are in the sky.
const CLOUDS: u64 = 34;
/// Clouds float between these distances from the eye, and between these
/// angles above the horizon.
pub const CLOUDS_FROM_M: f32 = 1_800.0;
pub const CLOUDS_TO_M: f32 = 3_400.0;
const CLOUD_LOW_DEG: f32 = 7.0;
const CLOUD_HIGH_DEG: f32 = 32.0;

/// The clouds, seeded by `seed`: each a few white blocks stuck together.
pub fn clouds(seed: u64) -> Mesh {
    let mut m = Mesh::default();
    let white = palette::BASE.cloud;
    let colour = [white[0], white[1], white[2], MATERIAL_CLOUD];
    for k in 0..CLOUDS {
        let h = splitmix(seed ^ 0xC10D ^ k);
        let azimuth = unit(h) * std::f32::consts::TAU;
        let dist = CLOUDS_FROM_M + (CLOUDS_TO_M - CLOUDS_FROM_M) * unit(h.rotate_left(7));
        let rise = (CLOUD_LOW_DEG + (CLOUD_HIGH_DEG - CLOUD_LOW_DEG) * unit(h.rotate_left(13)))
            .to_radians()
            .tan();
        let (cx, cy, cz) = (dist * azimuth.cos(), dist * rise, dist * azimuth.sin());
        let size = 70.0 + 110.0 * unit(h.rotate_left(19));
        let blocks = 2 + (h.rotate_left(25) % 3);
        for b in 0..blocks {
            let g = splitmix(h ^ (b + 1));
            let (w, ht, d) = (
                size * (0.6 + 0.5 * unit(g)),
                size * (0.35 + 0.25 * unit(g.rotate_left(5))),
                size * (0.5 + 0.4 * unit(g.rotate_left(11))),
            );
            let (ox, oy, oz) = (
                (unit(g.rotate_left(17)) - 0.5) * size * 0.9,
                (unit(g.rotate_left(23)) - 0.5) * size * 0.25,
                (unit(g.rotate_left(29)) - 0.5) * size * 0.6,
            );
            let (x, y, z) = (cx + ox, cy + oy, cz + oz);
            m.extend(cuboid(
                [x - w / 2.0, y - ht / 2.0, z - d / 2.0],
                [x + w / 2.0, y + ht / 2.0, z + d / 2.0],
                colour,
            ));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn horizontal(p: [f32; 3]) -> f32 {
        p[0].hypot(p[2])
    }

    #[test]
    fn should_ring_the_horizon_with_mountains_beyond_the_land() {
        // The land reaches 3.2 km from the road: every mountain stands past it.
        let m = mountains(7);
        assert!(!m.vertices.is_empty());
        for v in &m.vertices {
            assert!(horizontal(v.pos) > 3_200.0 - 0.1, "{:?}", v.pos);
            assert_eq!(v.colour[3], MATERIAL_BACKDROP);
        }
        // All the way round: every eighth of the horizon has a mountain.
        for octant in 0..8 {
            let any = m.vertices.iter().any(|v| {
                let a = v.pos[2].atan2(v.pos[0]).rem_euclid(std::f32::consts::TAU);
                (a / (std::f32::consts::TAU / 8.0)) as usize == octant
            });
            assert!(any, "a bare stretch of horizon in octant {octant}");
        }
    }

    #[test]
    fn should_cap_only_tall_peaks_with_snow() {
        let m = mountains(11);
        let snow = palette::BASE.mountain_snow;
        let snowy: Vec<&Vertex> = m
            .vertices
            .iter()
            .filter(|v| v.colour[..3] == snow)
            .collect();
        assert!(!snowy.is_empty(), "no snow anywhere");
        assert!(snowy
            .iter()
            .all(|v| v.pos[1] >= FOOT_M + SNOW_FROM_M * 0.66 - 1.0));
    }

    #[test]
    fn should_float_clouds_in_the_sky_not_on_the_ground() {
        let m = clouds(3);
        assert!(!m.vertices.is_empty());
        for v in &m.vertices {
            assert!(v.pos[1] > 50.0, "a cloud at {:?}", v.pos);
            assert_eq!(v.colour[3], MATERIAL_CLOUD);
        }
    }

    #[test]
    fn should_build_the_same_sky_for_the_same_seed() {
        assert_eq!(mountains(5), mountains(5));
        assert_eq!(clouds(5), clouds(5));
        assert_ne!(clouds(5), clouds(6));
        let n = mountains(5).vertices.len() as u32;
        assert!(mountains(5).indices.iter().all(|&i| i < n));
    }
}
