//! Animals: sheep grazing in flocks beside the road, birds circling over the
//! fields (the stylized-route skill, §7.8).
//!
//! Animals are props (`data/props.rs`): their meshes are drawn many times
//! and moved on the GPU by their [`Motion`]. They are scenery only — they
//! keep well clear of the road and never touch the ride (rule 5).
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::biome::BiomeDef;
use crate::data::course3d::{Mesh, RoadPoint, Vertex, MATERIAL_PLAIN};
use crate::data::palette;
use crate::data::props::{Motion, Prop, Shape, ORBIT_M, OWN_COLOUR};
use crate::data::world::{beside, splitmix, AreaKind};

/// No animal comes nearer the road's edge than this (the skill's 18 m).
pub const CLEAR_OF_ROAD_M: f32 = 18.0;

/// A flock may graze every this many metres of route, on one side or the
/// other.
const FLOCK_EVERY_M: f32 = 110.0;
/// Birds circle over the land every this many metres of route.
const BIRDS_EVERY_M: f32 = 450.0;

/// The x a sheep's head starts at, in mesh units: the shader dips every
/// vertex past this while it grazes.
pub const HEAD_FROM: f32 = 0.46;

/// A box from `lo` to `hi`, every face its own four corners so each is flat.
pub(crate) fn cuboid(lo: [f32; 3], hi: [f32; 3], colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let c = |x: usize, y: usize, z: usize| {
        [
            if x == 0 { lo[0] } else { hi[0] },
            if y == 0 { lo[1] } else { hi[1] },
            if z == 0 { lo[2] } else { hi[2] },
        ]
    };
    let faces = [
        [c(0, 0, 0), c(1, 0, 0), c(1, 1, 0), c(0, 1, 0)],
        [c(0, 0, 1), c(0, 1, 1), c(1, 1, 1), c(1, 0, 1)],
        [c(0, 0, 0), c(0, 1, 0), c(0, 1, 1), c(0, 0, 1)],
        [c(1, 0, 0), c(1, 0, 1), c(1, 1, 1), c(1, 1, 0)],
        [c(0, 1, 0), c(1, 1, 0), c(1, 1, 1), c(0, 1, 1)],
        [c(0, 0, 0), c(0, 0, 1), c(1, 0, 1), c(1, 0, 0)],
    ];
    for face in faces {
        let v = m.vertices.len() as u32;
        for pos in face {
            m.vertices.push(Vertex { pos, colour });
        }
        m.indices
            .extend_from_slice(&[v, v + 1, v + 2, v, v + 2, v + 3]);
    }
    m
}

/// An animal's mesh, one unit long, facing +x. Fleece takes the animal's own
/// colour.
pub fn mesh(shape: Shape) -> Mesh {
    let dark = palette::BASE.animal_dark;
    let dark = [dark[0], dark[1], dark[2], MATERIAL_PLAIN];
    let own = [OWN_COLOUR[0], OWN_COLOUR[1], OWN_COLOUR[2], MATERIAL_PLAIN];
    let mut m = Mesh::default();
    match shape {
        Shape::Sheep => {
            m.extend(cuboid([-0.45, 0.36, -0.28], [0.45, 0.92, 0.28], own));
            m.extend(cuboid(
                [HEAD_FROM + 0.01, 0.55, -0.15],
                [0.76, 0.88, 0.15],
                dark,
            ));
            for (x, z) in [(-0.3, -0.17), (-0.3, 0.17), (0.3, -0.17), (0.3, 0.17)] {
                m.extend(cuboid(
                    [x - 0.06, 0.0, z - 0.06],
                    [x + 0.06, 0.38, z + 0.06],
                    dark,
                ));
            }
        }
        Shape::Bird => {
            // The prop's own colour: dark birds inland, white gulls.
            let bird = own;
            m.extend(cuboid([-0.25, -0.05, -0.06], [0.25, 0.05, 0.06], bird));
            // Wings: thin slabs out to either side; the shader beats them.
            m.extend(cuboid([-0.1, -0.01, 0.06], [0.14, 0.01, 0.62], bird));
            m.extend(cuboid([-0.1, -0.01, -0.62], [0.14, 0.01, -0.06], bird));
        }
        _ => {}
    }
    m
}

/// What [`place`] needs to know about the world.
pub struct Land<'a> {
    pub seed: u64,
    /// How far `(x, z)` is from the road's edge.
    pub from_road: &'a dyn Fn(f32, f32) -> f32,
    pub area: &'a dyn Fn(f32, f32) -> Option<AreaKind>,
    /// The drawn ground's height, where there is ground.
    pub ground: &'a dyn Fn(f32, f32) -> Option<f32>,
    /// Taken by something else: a stream, a house.
    pub taken: &'a dyn Fn(f32, f32) -> bool,
    /// Which biome's herds and birds are at `(x, z)`.
    pub biome: &'a dyn Fn(f32, f32) -> &'static BiomeDef,
}

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Every animal along `road`: flocks of sheep on grass beside it, and birds
/// circling further out.
pub fn place(road: &[RoadPoint], land: &Land) -> Vec<Prop> {
    let mut out = Vec::new();
    let (Some(first), Some(last)) = (road.first(), road.last()) else {
        return out;
    };
    let mut k = 0u64;
    let mut along = first.distance_m + FLOCK_EVERY_M / 2.0;
    while along < last.distance_m {
        let h = splitmix(land.seed ^ 0x5EE9 ^ k);
        k += 1;
        let here = along;
        along += FLOCK_EVERY_M;
        let side = if h & 2 == 0 { 1.0 } else { -1.0 };
        let off = side * (23.0 + 22.0 * unit(h.rotate_left(11)));
        let Some((cx, cz)) = beside(road, here, off) else {
            continue;
        };
        let rules = &(land.biome)(cx, cz).animals;
        let Some(herd) = rules.herd else {
            continue;
        };
        if unit(h) >= rules.herd_chance {
            continue;
        }
        let [lo, hi] = rules.herd_count;
        let count = (lo + (h.rotate_left(23) % u64::from(hi - lo + 1)) as u32) as usize;
        let def = (land.biome)(cx, cz);
        for s in 0..count {
            let g = splitmix(h ^ (s as u64 + 1));
            let r = 9.0 * unit(g).sqrt();
            let a = unit(g.rotate_left(13)) * std::f32::consts::TAU;
            let (x, z) = (cx + r * a.cos(), cz + r * a.sin());
            if (land.from_road)(x, z) < CLEAR_OF_ROAD_M || (land.taken)(x, z) {
                continue;
            }
            match (land.area)(x, z) {
                Some(AreaKind::Water | AreaKind::Sand | AreaKind::Paving) => continue,
                Some(AreaKind::Wood) if !rules.herd_in_woods => continue,
                _ => {}
            }
            let Some(y) = (land.ground)(x, z) else {
                continue;
            };
            let coat = &rules.coats[(g % 3) as usize % rules.coats.len()];
            out.push(Prop {
                shape: herd,
                pos: [x, y, z],
                size: rules.herd_size_m * (0.9 + 0.2 * unit(g.rotate_left(29))),
                yaw: unit(g.rotate_left(37)) * std::f32::consts::TAU,
                colour: def.palette.named(coat).unwrap_or(def.palette.fleece_a),
                motion: Motion::Graze,
                phase: unit(g.rotate_left(47)) * std::f32::consts::TAU,
            });
        }
    }
    let mut along = first.distance_m + BIRDS_EVERY_M / 2.0;
    while along < last.distance_m {
        let h = splitmix(land.seed ^ 0xB12D ^ k);
        k += 1;
        let here = along;
        along += BIRDS_EVERY_M;
        let side = if h & 2 == 0 { 1.0 } else { -1.0 };
        let off = side * (70.0 + 200.0 * unit(h.rotate_left(7)));
        let Some((cx, cz)) = beside(road, here, off) else {
            continue;
        };
        // The whole circle keeps clear of the road.
        if (land.from_road)(cx, cz) < CLEAR_OF_ROAD_M + ORBIT_M {
            continue;
        }
        let def = (land.biome)(cx, cz);
        if !def.animals.birds {
            continue;
        }
        let Some(y) = (land.ground)(cx, cz) else {
            continue;
        };
        let lift = 25.0 + 20.0 * unit(h.rotate_left(19));
        let [lo, hi] = def.animals.bird_flock;
        let count = (lo + (h.rotate_left(31) % u64::from(hi - lo + 1)) as u32) as usize;
        for b in 0..count {
            let g = splitmix(h ^ (b as u64 + 7));
            out.push(Prop {
                shape: Shape::Bird,
                pos: [cx, y + lift + 4.0 * unit(g), cz],
                size: def.animals.bird_size_m,
                yaw: 0.0,
                colour: def.palette.bird,
                motion: Motion::Orbit,
                // Spread round the circle: a loose flock.
                phase: b as f32 * 0.5 + unit(g.rotate_left(9)) * 0.3,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn north_road(points: usize) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 10.0],
                distance_m: i as f32 * 10.0,
            })
            .collect()
    }

    fn from_road(x: f32, _: f32) -> f32 {
        (x.abs() - 2.6).max(0.0)
    }
    fn grass(_: f32, _: f32) -> Option<AreaKind> {
        None
    }
    fn ground(x: f32, _: f32) -> Option<f32> {
        Some(10.0 + x * 0.01)
    }
    fn free(_: f32, _: f32) -> bool {
        false
    }

    fn temperate(_: f32, _: f32) -> &'static BiomeDef {
        crate::data::biome::Biome::Temperate.def()
    }

    fn land(seed: u64) -> Land<'static> {
        Land {
            seed,
            from_road: &from_road,
            area: &grass,
            ground: &ground,
            taken: &free,
            biome: &temperate,
        }
    }

    #[test]
    fn should_keep_every_animal_and_every_circle_off_the_road() {
        // Rule 5: animals never come near the ride. A bird's whole circle,
        // not just its middle, stays clear.
        for seed in 0..20 {
            let props = place(&north_road(1_000), &land(seed));
            for p in &props {
                let reach = if p.motion == Motion::Orbit {
                    ORBIT_M
                } else {
                    0.0
                };
                assert!(
                    from_road(p.pos[0], p.pos[2]) - reach >= CLEAR_OF_ROAD_M,
                    "{p:?}"
                );
            }
        }
    }

    #[test]
    fn should_graze_flocks_on_the_ground_and_fly_birds_well_above_it() {
        let props = place(&north_road(1_000), &land(4));
        let sheep: Vec<&Prop> = props.iter().filter(|p| p.shape == Shape::Sheep).collect();
        let birds: Vec<&Prop> = props.iter().filter(|p| p.shape == Shape::Bird).collect();
        // 10 km: about 40 flocks of 3–8.
        assert!((60..=500).contains(&sheep.len()), "{} sheep", sheep.len());
        assert!(birds.len() >= 20, "{} birds", birds.len());
        for s in &sheep {
            assert_eq!(s.pos[1], ground(s.pos[0], s.pos[2]).expect("ground"));
            assert_eq!(s.motion, Motion::Graze);
        }
        for b in &birds {
            assert!(b.pos[1] >= ground(b.pos[0], b.pos[2]).expect("ground") + 25.0);
        }
    }

    #[test]
    fn should_keep_sheep_out_of_water_woods_and_streams() {
        fn wet(x: f32, _: f32) -> Option<AreaKind> {
            if x > 0.0 {
                Some(AreaKind::Water)
            } else {
                Some(AreaKind::Wood)
            }
        }
        let l = Land {
            area: &wet,
            ..land(1)
        };
        assert!(place(&north_road(500), &l)
            .iter()
            .all(|p| p.shape != Shape::Sheep));
        fn stream(_: f32, _: f32) -> bool {
            true
        }
        let l = Land {
            taken: &stream,
            ..land(1)
        };
        assert!(place(&north_road(500), &l)
            .iter()
            .all(|p| p.shape != Shape::Sheep));
    }

    #[test]
    fn should_place_the_same_animals_every_time() {
        let road = north_road(600);
        assert_eq!(place(&road, &land(9)), place(&road, &land(9)));
        assert_ne!(place(&road, &land(9)), place(&road, &land(10)));
    }

    #[test]
    fn should_survive_no_road_and_a_road_of_one_spot() {
        assert!(place(&[], &land(1)).is_empty());
        let spot = vec![
            RoadPoint {
                pos: [0.0; 3],
                distance_m: 0.0,
            };
            50
        ];
        assert!(place(&spot, &land(1)).is_empty());
    }

    #[test]
    fn should_put_a_sheeps_head_forward_of_everything_else() {
        let m = mesh(Shape::Sheep);
        let dark = palette::BASE.animal_dark;
        for v in &m.vertices {
            let is_head = v.pos[0] > HEAD_FROM;
            // Only the head lies past the line the shader dips.
            if is_head {
                assert_eq!(v.colour[..3], dark);
                assert!(v.pos[1] > 0.5);
            }
        }
        assert!(m.vertices.iter().any(|v| v.pos[0] > HEAD_FROM));
    }
}
