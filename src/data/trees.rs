//! Trees: three low-poly shapes, and where they grow along the route.
//!
//! Trees are props (`data/props.rs`): one mesh per shape, drawn many times.
//!
//! Woods are planted densely, open country sparsely, rows of poplars where
//! the world lays them — all by the biome's rules (`biomes.json`), all
//! seeded by place, so a route has the same trees every ride (the
//! stylized-route skill, §6 and rule 6). Trees are scenery only: nothing here
//! reaches the ride.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::biome::{self, BiomeDef};
use crate::data::course3d::{Mesh, Vertex, MATERIAL_GROUND, MATERIAL_PLAIN};
use crate::data::palette;
use crate::data::props::{Motion, Prop, Shape, OWN_COLOUR};
use crate::data::world::AreaKind;

/// The tree shapes.
#[cfg(test)]
pub const SHAPES: [Shape; 3] = [Shape::Round, Shape::Cone, Shape::Poplar];

/// A crown vertex: painted with the tree's own colour.
const CROWN: [f32; 4] = [OWN_COLOUR[0], OWN_COLOUR[1], OWN_COLOUR[2], MATERIAL_GROUND];

/// Typical height of a tree shape, in metres, before a tree's own scale.
fn height_m(shape: Shape) -> f32 {
    match shape {
        Shape::Cone => 10.0,
        Shape::Poplar => 13.0,
        Shape::Olive => 6.0,
        Shape::Cypress => 11.0,
        Shape::Palm => 9.0,
        Shape::Birch => 10.0,
        _ => 8.0,
    }
}

/// Sides of a trunk, a cone or a ring of a crown: the skill's 6–10.
const SIDES: usize = 7;

/// A tree shape's mesh, one unit tall, standing on the origin. Crown vertices
/// take the tree's own colour; the trunk is the palette's. Not a tree shape:
/// no mesh.
pub fn mesh(kind: Shape) -> Mesh {
    let t = palette::BASE.trunk;
    let trunk = [t[0], t[1], t[2], MATERIAL_PLAIN];
    let mut m = Mesh::default();
    match kind {
        Shape::Round => {
            m.extend(prism(0.035, 0.0, 0.42, trunk));
            m.extend(ball([0.0, 0.66, 0.0], [0.3, 0.32, 0.3], CROWN));
        }
        Shape::Cone => {
            m.extend(prism(0.035, 0.0, 0.24, trunk));
            m.extend(cone(0.17, 0.36, 0.72, CROWN));
            m.extend(cone(0.48, 0.25, 1.0, CROWN));
        }
        Shape::Poplar => {
            m.extend(prism(0.022, 0.0, 0.2, trunk));
            m.extend(ball([0.0, 0.58, 0.0], [0.13, 0.42, 0.13], CROWN));
        }
        _ => {}
    }
    m
}

fn ring(radius: f32, y: f32) -> impl Iterator<Item = [f32; 3]> {
    (0..SIDES).map(move |k| {
        let a = k as f32 / SIDES as f32 * std::f32::consts::TAU;
        [radius * a.cos(), y, radius * a.sin()]
    })
}

fn push(m: &mut Mesh, pos: [f32; 3], colour: [f32; 4]) -> u32 {
    m.vertices.push(Vertex { pos, colour });
    m.vertices.len() as u32 - 1
}

/// An upright prism: a trunk.
fn prism(radius: f32, bottom: f32, top: f32, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let lo: Vec<u32> = ring(radius, bottom)
        .map(|p| push(&mut m, p, colour))
        .collect();
    let hi: Vec<u32> = ring(radius, top).map(|p| push(&mut m, p, colour)).collect();
    for k in 0..SIDES {
        let j = (k + 1) % SIDES;
        m.indices
            .extend_from_slice(&[lo[k], lo[j], hi[j], lo[k], hi[j], hi[k]]);
    }
    m
}

/// A cone from a ring at `base` up to a point at `tip`, closed underneath.
fn cone(base: f32, radius: f32, tip: f32, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let rim: Vec<u32> = ring(radius, base)
        .map(|p| push(&mut m, p, colour))
        .collect();
    let apex = push(&mut m, [0.0, tip, 0.0], colour);
    let under = push(&mut m, [0.0, base, 0.0], colour);
    for k in 0..SIDES {
        let j = (k + 1) % SIDES;
        m.indices
            .extend_from_slice(&[rim[k], rim[j], apex, rim[j], rim[k], under]);
    }
    m
}

/// A low-poly ball: three rings between two poles, stretched by `radius`.
pub(crate) fn ball(centre: [f32; 3], radius: [f32; 3], colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let at = |x: f32, y: f32, z: f32| {
        [
            centre[0] + x * radius[0],
            centre[1] + y * radius[1],
            centre[2] + z * radius[2],
        ]
    };
    let bottom = push(&mut m, at(0.0, -1.0, 0.0), colour);
    let rings: Vec<Vec<u32>> = [-0.55f32, 0.15, 0.75]
        .iter()
        .enumerate()
        .map(|(r, &y)| {
            let w = (1.0 - y * y).sqrt();
            // Every other ring turned half a side, so the facets interlock.
            let turn = r as f32 * 0.5;
            (0..SIDES)
                .map(|k| {
                    let a = (k as f32 + turn) / SIDES as f32 * std::f32::consts::TAU;
                    push(&mut m, at(w * a.cos(), y, w * a.sin()), colour)
                })
                .collect()
        })
        .collect();
    let top = push(&mut m, at(0.0, 1.0, 0.0), colour);
    for k in 0..SIDES {
        let j = (k + 1) % SIDES;
        m.indices
            .extend_from_slice(&[bottom, rings[0][j], rings[0][k]]);
        for r in 0..rings.len() - 1 {
            let (a, b) = (&rings[r], &rings[r + 1]);
            m.indices
                .extend_from_slice(&[a[k], a[j], b[k], a[j], b[j], b[k]]);
        }
        let last = &rings[rings.len() - 1];
        m.indices.extend_from_slice(&[last[k], last[j], top]);
    }
    m
}

/// Trees are planted only this near the road: past it the haze has them.
pub const REACH_M: f32 = 1_100.0;

/// No tree stands closer than this to the road's edge — a crown over the
/// track would be in the rider's face, and round a tight bend the camera
/// swings out a few metres past the edge.
pub const CLEAR_OF_ROAD_M: f32 = 8.0;

/// The land is looked at in cells this wide: one draw for the open-country
/// lone tree, a few planting spots in a wood.
const CELL_M: f32 = 20.0;

/// Trees along a row stand this far apart.
const ROW_SPACING_M: f32 = 9.0;

/// What [`scatter`] needs to know about the land.
pub struct Land<'a> {
    /// The scene's extent to plant over: x then z, low to high.
    pub bounds: ((f32, f32), (f32, f32)),
    /// How far `(x, z)` is from the road's edge, or `None` past [`REACH_M`].
    pub from_road: &'a dyn Fn(f32, f32) -> Option<f32>,
    pub area: &'a dyn Fn(f32, f32) -> Option<AreaKind>,
    /// The drawn ground's height, where there is ground.
    pub ground: &'a dyn Fn(f32, f32) -> Option<f32>,
    /// Taken by something else — a stream, a sheep's patch of grass.
    pub taken: &'a dyn Fn(f32, f32) -> bool,
    pub biome: &'a dyn Fn(f32, f32) -> &'static BiomeDef,
    /// Rows of poplars and lone trees the world lays out, in scene space.
    pub rows: &'a [Vec<(f32, f32)>],
    pub lone: &'a [(f32, f32)],
    /// Thin woods out from this far, in place of each biome's own rule.
    pub thin_from_m: Option<f32>,
}

/// Every tree along the route.
pub fn scatter(land: &Land) -> Vec<Prop> {
    let mut trees = Vec::new();
    let ((x_lo, x_hi), (z_lo, z_hi)) = land.bounds;
    if !(x_lo.is_finite() && x_hi.is_finite() && z_lo.is_finite() && z_hi.is_finite()) {
        return trees;
    }
    let (i_lo, i_hi) = (
        (x_lo / CELL_M).floor() as i64,
        (x_hi / CELL_M).ceil() as i64,
    );
    let (j_lo, j_hi) = (
        (z_lo / CELL_M).floor() as i64,
        (z_hi / CELL_M).ceil() as i64,
    );
    for i in i_lo..i_hi {
        for j in j_lo..j_hi {
            let (cx, cz) = ((i as f32 + 0.5) * CELL_M, (j as f32 + 0.5) * CELL_M);
            // A whole cell further than its half-diagonal past the reach
            // cannot hold a tree.
            if (land.from_road)(cx, cz).is_none() {
                continue;
            }
            let def = (land.biome)(cx, cz);
            if (land.area)(cx, cz) == Some(AreaKind::Wood) {
                // A wood: a jittered grid at the biome's spacing, which keeps
                // trees apart as a Poisson-disc scatter would, at a fraction
                // of the cost.
                let [lo, hi] = def.trees.forest_spacing_m;
                let spacing =
                    lo + (hi - lo) * biome::unit(biome::seed_at(cx, cz, CELL_M, 0x5_9AC3));
                let k = (CELL_M / spacing).round().max(1.0) as usize;
                let from = (land.from_road)(cx, cz).unwrap_or(f32::MAX);
                let thin = land.thin_from_m.unwrap_or(def.trees.thin_from_m);
                let k = if from > 4.0 * thin {
                    k.div_ceil(4)
                } else if from > 2.0 * thin {
                    k.div_ceil(3)
                } else if from > thin {
                    k.div_ceil(2)
                } else {
                    k
                };
                let step = CELL_M / k as f32;
                for a in 0..k {
                    for b in 0..k {
                        let seed = biome::seed_at(
                            i as f32 * CELL_M + a as f32 * step,
                            j as f32 * CELL_M + b as f32 * step,
                            1.0,
                            0x7_12EE,
                        );
                        let jx = (biome::unit(seed) - 0.5) * 0.7 * step;
                        let jz = (biome::unit(seed.rotate_left(21)) - 0.5) * 0.7 * step;
                        let x = i as f32 * CELL_M + (a as f32 + 0.5) * step + jx;
                        let z = j as f32 * CELL_M + (b as f32 + 0.5) * step + jz;
                        if (land.area)(x, z) == Some(AreaKind::Wood) {
                            plant(land, &mut trees, x, z, seed, None);
                        }
                    }
                }
            } else {
                // Open country: now and then a lone tree, as many per
                // hectare as the biome grows.
                let seed = biome::seed_at(cx, cz, CELL_M, 0x0_FE17);
                let chance = def.trees.open_per_hectare * CELL_M * CELL_M / 10_000.0;
                if biome::unit(seed) < chance {
                    let x = cx + (biome::unit(seed.rotate_left(13)) - 0.5) * CELL_M;
                    let z = cz + (biome::unit(seed.rotate_left(29)) - 0.5) * CELL_M;
                    if !matches!((land.area)(x, z), Some(AreaKind::Water)) {
                        plant(land, &mut trees, x, z, seed, None);
                    }
                }
            }
        }
    }
    for row in land.rows {
        for w in row.windows(2) {
            let ((ax, az), (bx, bz)) = (w[0], w[1]);
            let len = (bx - ax).hypot(bz - az);
            if !len.is_finite() {
                continue;
            }
            let n = (len / ROW_SPACING_M).floor() as usize;
            for k in 0..=n {
                let t = if n == 0 { 0.0 } else { k as f32 / n as f32 };
                let (x, z) = (ax + (bx - ax) * t, az + (bz - az) * t);
                let seed = biome::seed_at(x, z, 1.0, 0x0_2087);
                plant(land, &mut trees, x, z, seed, Some(Shape::Poplar));
            }
        }
    }
    for &(x, z) in land.lone {
        let seed = biome::seed_at(x, z, 1.0, 0x0_10AE);
        plant(land, &mut trees, x, z, seed, None);
    }
    trees
}

/// Plant one tree at `(x, z)` if the place allows it: near enough the road
/// to be seen, clear of the road itself, on ground, out of buildings and
/// water.
fn plant(land: &Land, trees: &mut Vec<Prop>, x: f32, z: f32, seed: u64, kind: Option<Shape>) {
    if !(x.is_finite() && z.is_finite()) {
        return;
    }
    let Some(from_road) = (land.from_road)(x, z) else {
        return;
    };
    if from_road < CLEAR_OF_ROAD_M || (land.taken)(x, z) {
        return;
    }
    if (land.area)(x, z) == Some(AreaKind::Water) {
        return;
    }
    let Some(y) = (land.ground)(x, z) else {
        return;
    };
    let def = (land.biome)(x, z);
    let kind = kind.unwrap_or_else(|| {
        if biome::unit(seed.rotate_left(7)) < def.trees.cone_share {
            def.trees.kinds[1]
        } else {
            def.trees.kinds[0]
        }
    });
    let scale = 0.8 + 0.5 * biome::unit(seed.rotate_left(37));
    trees.push(Prop {
        shape: kind,
        pos: [x, y, z],
        size: height_m(kind) * scale,
        yaw: biome::unit(seed.rotate_left(47)) * std::f32::consts::TAU,
        colour: def.palette.tree(seed.rotate_left(53)),
        motion: Motion::Still,
        phase: 0.0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::biome::Biome;

    #[test]
    fn should_stand_every_shape_on_the_ground_one_unit_tall() {
        for kind in SHAPES {
            let m = mesh(kind);
            let lo = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MAX, f32::min);
            let hi = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MIN, f32::max);
            assert!(lo.abs() < 1e-6, "{kind:?} starts at {lo}");
            assert!((hi - 1.0).abs() < 0.03, "{kind:?} is {hi} tall");
            let n = m.vertices.len() as u32;
            assert!(m.indices.iter().all(|&i| i < n));
            assert_eq!(m.indices.len() % 3, 0);
        }
    }

    #[test]
    fn should_mark_crowns_for_the_trees_own_colour_and_keep_trunks_brown() {
        for kind in SHAPES {
            let m = mesh(kind);
            assert!(m.vertices.iter().any(|v| v.colour == CROWN));
            let trunk = palette::BASE.trunk;
            assert!(m.vertices.iter().any(|v| v.colour[..3] == trunk));
            // A crown never comes down to the ground.
            assert!(m
                .vertices
                .iter()
                .filter(|v| v.colour == CROWN)
                .all(|v| v.pos[1] > 0.15));
        }
    }

    #[test]
    fn should_keep_a_low_poly_count() {
        for kind in SHAPES {
            let tris = mesh(kind).indices.len() / 3;
            assert!(tris <= 80, "{kind:?} has {tris} triangles");
        }
    }

    /// A road along z = 0 (x east); a wood north of it from 50 m out, open
    /// land south, a house at x 0..10 south of the road.
    fn land_fixture<'a>(
        rows: &'a [Vec<(f32, f32)>],
        lone: &'a [(f32, f32)],
        biome: &'a dyn Fn(f32, f32) -> &'static BiomeDef,
    ) -> Land<'a> {
        fn from_road(_: f32, z: f32) -> Option<f32> {
            let d = z.abs() - 2.6;
            (d <= REACH_M).then_some(d.max(0.0))
        }
        fn area(_: f32, z: f32) -> Option<AreaKind> {
            (z < -50.0).then_some(AreaKind::Wood)
        }
        fn ground(x: f32, _: f32) -> Option<f32> {
            Some(x * 0.01)
        }
        fn taken(x: f32, z: f32) -> bool {
            (0.0..10.0).contains(&x) && (20.0..30.0).contains(&z)
        }
        Land {
            bounds: ((-500.0, 500.0), (-900.0, 900.0)),
            from_road: &from_road,
            area: &area,
            ground: &ground,
            taken: &taken,
            biome,
            rows,
            lone,
            thin_from_m: None,
        }
    }

    fn temperate(_: f32, _: f32) -> &'static BiomeDef {
        Biome::Temperate.def()
    }

    #[test]
    fn should_plant_woods_densely_at_the_biomes_spacing() {
        let land = land_fixture(&[], &[], &temperate);
        let trees = scatter(&land);
        let in_wood: Vec<&Prop> = trees.iter().filter(|t| t.pos[2] < -50.0).collect();
        // Near the road, 6–10 m spacing: one tree per 36–100 m². Past the
        // thinning line, about a quarter of that. Counted per hectare in a
        // band of each, 1000 m wide and 250 m deep.
        let per_ha = |lo: f32, hi: f32| {
            let n = in_wood
                .iter()
                .filter(|t| (lo..hi).contains(&(-t.pos[2] - 2.6)))
                .count();
            n as f32 / (1_000.0 * (hi - lo) / 10_000.0)
        };
        let near = per_ha(80.0, 330.0);
        let far = per_ha(500.0, 750.0);
        assert!(
            (100.0..=280.0).contains(&near),
            "{near} trees per hectare near"
        );
        assert!((15.0..=90.0).contains(&far), "{far} trees per hectare far");
        // No two trees on the same spot.
        let mut spots: Vec<(i32, i32)> = in_wood
            .iter()
            .map(|t| ((t.pos[0] * 2.0) as i32, (t.pos[2] * 2.0) as i32))
            .collect();
        spots.sort_unstable();
        let n = spots.len();
        spots.dedup();
        assert_eq!(spots.len(), n);
    }

    #[test]
    fn should_plant_open_land_sparsely() {
        // About as many lone trees as the biome grows per hectare: 1000 m by
        // the 800 m or so of open land within reach.
        let land = land_fixture(&[], &[], &temperate);
        let open = scatter(&land).iter().filter(|t| t.pos[2] > 0.0).count() as f32;
        let want = Biome::Temperate.def().trees.open_per_hectare * 1_000.0 * 1_090.0 / 10_000.0;
        assert!(
            (want * 0.6..=want * 1.4).contains(&open),
            "{open} lone trees, want ~{want}"
        );
    }

    #[test]
    fn should_keep_every_tree_off_the_road_out_of_buildings_and_on_the_ground() {
        let rows = vec![
            vec![(-300.0, 3.0), (300.0, 3.0)],
            vec![(-300.0, 12.0), (300.0, 12.0)],
        ];
        let lone = vec![(5.0, 25.0), (100.0, 1.0), (40.0, 40.0)];
        let land = land_fixture(&rows, &lone, &temperate);
        let trees = scatter(&land);
        assert!(!trees.is_empty());
        for t in &trees {
            let (x, z) = (t.pos[0], t.pos[2]);
            assert!(z.abs() - 2.6 >= CLEAR_OF_ROAD_M, "on the road: {t:?}");
            assert!(!(land.taken)(x, z), "in a house: {t:?}");
            assert!((t.pos[1] - x * 0.01).abs() < 1e-4, "off the ground: {t:?}");
            assert!(z.abs() - 2.6 <= REACH_M);
        }
        // The row hard against the road is not planted; the one 12 m out is.
        assert!(trees.iter().all(|t| (t.pos[2] - 3.0).abs() > 1e-3));
        assert!(trees.iter().filter(|t| t.shape == Shape::Poplar).count() >= 60);
        // The lone tree in open land stands; the one in the house does not.
        assert!(trees.iter().any(|t| t.pos[0] == 40.0 && t.pos[2] == 40.0));
        assert!(!trees.iter().any(|t| t.pos[0] == 5.0 && t.pos[2] == 25.0));
    }

    #[test]
    fn should_grow_the_same_trees_every_time() {
        let rows = vec![vec![(-300.0, 30.0), (300.0, 30.0)]];
        let land = land_fixture(&rows, &[(40.0, 40.0)], &temperate);
        assert_eq!(scatter(&land), scatter(&land));
    }

    #[test]
    fn should_grow_mostly_conifers_in_the_mountains() {
        let mountain = |_: f32, _: f32| Biome::Mountain.def();
        let land = land_fixture(&[], &[], &mountain);
        let trees = scatter(&land);
        let cones = trees.iter().filter(|t| t.shape == Shape::Cone).count();
        assert!(
            cones as f32 > 0.8 * trees.len() as f32,
            "{cones} of {}",
            trees.len()
        );
        let colours = [
            Biome::Mountain.def().palette.tree_a,
            Biome::Mountain.def().palette.tree_b,
            Biome::Mountain.def().palette.tree_c,
        ];
        assert!(trees.iter().all(|t| colours.contains(&t.colour)));
    }

    #[test]
    fn should_vary_height_and_turn_within_bounds() {
        let land = land_fixture(&[], &[], &temperate);
        let trees = scatter(&land);
        for t in &trees {
            let base = height_m(t.shape);
            assert!(
                t.size >= base * 0.8 - 1e-4 && t.size <= base * 1.3 + 1e-4,
                "{t:?}"
            );
            assert!((0.0..std::f32::consts::TAU).contains(&t.yaw));
        }
    }

    #[test]
    fn should_plant_nothing_in_a_world_of_nan() {
        let mut land = land_fixture(&[], &[], &temperate);
        land.bounds = ((f32::NAN, 0.0), (0.0, 1.0));
        assert!(scatter(&land).is_empty());
        let bad_rows = vec![vec![(f32::NAN, 0.0), (f32::INFINITY, 1.0)]];
        let land = Land {
            bounds: ((0.0, 0.0), (0.0, 0.0)),
            ..land_fixture(&bad_rows, &[(f32::NAN, 5.0)], &temperate)
        };
        assert!(scatter(&land).is_empty());
    }
}
