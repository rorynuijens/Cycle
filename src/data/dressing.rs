//! Dressing: everything a biome sets about its land besides its trees and
//! its herds — cacti and boulders, reeds by the water, hay bales in the
//! fields, barns, houses and villas facing the road, a windmill now and
//! then, a lighthouse on the beach, boats on the sea, ducks on the pools
//! (the stylized-route skill, §7.5).
//!
//! What goes where is data: each biome lists its [`PropRule`]s in
//! `biomes.json`. Every choice is seeded by place or by distance along the
//! route, so a route is dressed the same way every ride (rule 6), and
//! nothing here reaches the ride (rule 5).
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use std::collections::HashSet;

use serde::Deserialize;

use crate::data::biome::{self, Biome, BiomeDef};
use crate::data::course3d::RoadPoint;
use crate::data::props::{Motion, Prop, Shape};
use crate::data::world::{beside, splitmix, AreaKind};

/// What a prop must stand on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ground {
    /// Grass: not a field, a wood, sand, paving or water.
    Open,
    /// A field.
    Field,
    /// Grass or a field.
    Grassy,
    Wood,
    Sand,
    Paving,
    /// On open water.
    Water,
    /// Dry land at the water's edge: a pool's, the sea's, a ditch's.
    Shore,
    /// Anywhere that is not water.
    Dry,
}

/// How a prop moves, as the data file names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Moves {
    Still,
    Graze,
    Bob,
    Spin,
}

impl Moves {
    fn motion(self) -> Motion {
        match self {
            Moves::Still => Motion::Still,
            Moves::Graze => Motion::Graze,
            Moves::Bob => Motion::Bob,
            Moves::Spin => Motion::Spin,
        }
    }
}

fn one() -> f32 {
    1.0
}
fn single() -> [u32; 2] {
    [1, 1]
}
fn still() -> Moves {
    Moves::Still
}

/// One kind of prop a biome sets out, and the rules for where.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropRule {
    pub shape: Shape,
    /// Palette names its own colour is picked from; none, a tree's.
    #[serde(default)]
    pub colours: Vec<String>,
    /// Scattered over the land: groups per hectare.
    #[serde(default)]
    pub per_hectare: f32,
    /// Or set along the road: a group every this many metres of it …
    #[serde(default)]
    pub every_m: f32,
    /// … that many times in one.
    #[serde(default = "one")]
    pub chance: f32,
    /// On both sides of the road at each spot, not one of them.
    #[serde(default)]
    pub both_sides: bool,
    /// Between these distances from the road's edge, in metres.
    pub from_m: f32,
    pub to_m: f32,
    pub on: Ground,
    /// How many in a group, fewest and most, and how far they spread.
    #[serde(default = "single")]
    pub cluster: [u32; 2],
    #[serde(default)]
    pub spread_m: f32,
    /// Size in metres, least and most: the mesh's unit scaled to it.
    pub size: [f32; 2],
    #[serde(default = "still")]
    pub motion: Moves,
    /// Turned to face the road, as a house is.
    #[serde(default)]
    pub face_road: bool,
    /// The ground it takes for itself, as a radius in metres: nothing else
    /// that takes ground, no tree and no animal stands within it.
    #[serde(default)]
    pub claims_m: f32,
}

impl PropRule {
    /// Whether the numbers make sense — checked as the data file is read.
    pub fn is_sane(&self) -> bool {
        let finite = [
            self.per_hectare,
            self.every_m,
            self.chance,
            self.from_m,
            self.to_m,
            self.spread_m,
            self.size[0],
            self.size[1],
            self.claims_m,
        ]
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0);
        finite
            && (self.per_hectare > 0.0) != (self.every_m > 0.0)
            && (self.every_m == 0.0 || self.every_m >= 5.0)
            && self.per_hectare <= 50.0
            && self.chance <= 1.0
            && self.from_m <= self.to_m
            && self.to_m <= MAX_REACH_M
            && self.size[0] > 0.0
            && self.size[0] <= self.size[1]
            && (1..=self.cluster[1].max(1)).contains(&self.cluster[0])
            && self.cluster[1] <= 20
    }
}

/// No prop is set further from the road than this, in metres: past it the
/// haze has the land.
pub const MAX_REACH_M: f32 = 1_100.0;

/// The land is looked over in cells this wide for scattered props.
const CELL_M: f32 = 40.0;
/// Ground is claimed in cells this wide.
const CLAIM_CELL_M: f32 = 5.0;
/// Nothing that claims ground comes nearer the road's edge than this.
const CLAIM_CLEAR_M: f32 = 3.0;

/// What [`place`] needs to know about the land.
pub struct Land<'a> {
    pub seed: u64,
    /// The scene's extent: x then z, low to high.
    pub bounds: ((f32, f32), (f32, f32)),
    /// How far `(x, z)` is from the road's edge.
    pub from_road: &'a dyn Fn(f32, f32) -> f32,
    pub area: &'a dyn Fn(f32, f32) -> Option<AreaKind>,
    /// The drawn ground's height — on water, the water's.
    pub ground: &'a dyn Fn(f32, f32) -> Option<f32>,
    /// Taken already: a stream or a ditch.
    pub taken: &'a dyn Fn(f32, f32) -> bool,
    /// Which biome dresses `(x, z)`.
    pub biome: &'a dyn Fn(f32, f32) -> Biome,
    /// The way to the nearest road, as a yaw.
    pub toward_road: &'a dyn Fn(f32, f32) -> f32,
}

/// The props, and the ground they claimed.
pub struct Dressed {
    pub props: Vec<Prop>,
    claimed: HashSet<(i64, i64)>,
}

impl Dressed {
    /// Whether `(x, z)` is on ground a prop claimed.
    pub fn claimed(&self, x: f32, z: f32) -> bool {
        self.claimed.contains(&claim_cell(x, z))
    }
}

fn claim_cell(x: f32, z: f32) -> (i64, i64) {
    (
        (x / CLAIM_CELL_M).floor() as i64,
        (z / CLAIM_CELL_M).floor() as i64,
    )
}

/// Every biome's dressing along `road`.
pub fn place(road: &[RoadPoint], land: &Land) -> Dressed {
    let mut out = Dressed {
        props: Vec::new(),
        claimed: HashSet::new(),
    };
    let (Some(first), Some(last)) = (road.first(), road.last()) else {
        return out;
    };
    // Along the road first: the houses and landmarks claim their ground
    // before anything is scattered round them.
    for b in Biome::ALL {
        for (r, rule) in b.def().props.iter().enumerate() {
            if rule.every_m <= 0.0 {
                continue;
            }
            let salt = splitmix(land.seed ^ (b as u64) << 8 ^ r as u64 ^ 0xA10E);
            let mut k = 0u64;
            let mut along = first.distance_m + rule.every_m / 2.0;
            while along < last.distance_m {
                let h = splitmix(salt ^ k);
                k += 1;
                let here = along;
                along += rule.every_m;
                if biome::unit(h) >= rule.chance {
                    continue;
                }
                let sides: &[f32] = if rule.both_sides {
                    &[-1.0, 1.0]
                } else if h & 2 == 0 {
                    &[1.0]
                } else {
                    &[-1.0]
                };
                for &side in sides {
                    let g = splitmix(h ^ side.to_bits() as u64);
                    // Offsets are from the road's line; the track is
                    // half its width either side of it.
                    let off = crate::data::roadway::TRACK_HALF_WIDTH_M
                        + rule.from_m
                        + (rule.to_m - rule.from_m) * biome::unit(g.rotate_left(11));
                    let Some((x, z)) = beside(road, here, side * off) else {
                        continue;
                    };
                    if (land.biome)(x, z) != b {
                        continue;
                    }
                    group(land, rule, b.def(), x, z, g, &mut out);
                }
            }
        }
    }
    // Then scattered over the land, cell by cell.
    let ((x_lo, x_hi), (z_lo, z_hi)) = land.bounds;
    if !(x_lo.is_finite() && x_hi.is_finite() && z_lo.is_finite() && z_hi.is_finite()) {
        return out;
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
            if (land.from_road)(cx, cz) > MAX_REACH_M + CELL_M {
                continue;
            }
            let b = (land.biome)(cx, cz);
            let def = b.def();
            for (r, rule) in def.props.iter().enumerate() {
                if rule.per_hectare <= 0.0 {
                    continue;
                }
                let h = biome::seed_at(cx, cz, CELL_M, 0xD2E5 ^ (b as u64) << 8 ^ r as u64);
                let expect = rule.per_hectare * CELL_M * CELL_M / 10_000.0;
                let n = expect.floor() as u64 + u64::from(biome::unit(h) < expect.fract());
                for s in 0..n {
                    let g = splitmix(h ^ (s + 1));
                    let x = cx + (biome::unit(g.rotate_left(5)) - 0.5) * CELL_M;
                    let z = cz + (biome::unit(g.rotate_left(17)) - 0.5) * CELL_M;
                    group(land, rule, def, x, z, g, &mut out);
                }
            }
        }
    }
    out
}

/// A group of `rule`'s props round `(cx, cz)`.
fn group(
    land: &Land,
    rule: &PropRule,
    def: &BiomeDef,
    cx: f32,
    cz: f32,
    h: u64,
    out: &mut Dressed,
) {
    let [lo, hi] = rule.cluster;
    let count = lo + (splitmix(h ^ 0xC1) % u64::from(hi - lo + 1)) as u32;
    for s in 0..count {
        let g = splitmix(h ^ (u64::from(s) + 0x51));
        let (x, z) = if s == 0 && rule.spread_m <= 0.0 {
            (cx, cz)
        } else {
            let r = rule.spread_m * biome::unit(g).sqrt();
            let a = biome::unit(g.rotate_left(13)) * std::f32::consts::TAU;
            (cx + r * a.cos(), cz + r * a.sin())
        };
        if let Some(p) = one_prop(land, rule, def, x, z, g, out) {
            out.props.push(p);
        }
    }
}

/// One of `rule`'s props at `(x, z)`, if the place allows it.
fn one_prop(
    land: &Land,
    rule: &PropRule,
    def: &BiomeDef,
    x: f32,
    z: f32,
    g: u64,
    out: &mut Dressed,
) -> Option<Prop> {
    if !(x.is_finite() && z.is_finite()) {
        return None;
    }
    let from = (land.from_road)(x, z);
    if !(rule.from_m..=rule.to_m).contains(&from) {
        return None;
    }
    let area = (land.area)(x, z);
    let wet = |x: f32, z: f32| (land.area)(x, z) == Some(AreaKind::Water);
    let fits = match rule.on {
        Ground::Open => area.is_none(),
        Ground::Field => area == Some(AreaKind::Farmland),
        Ground::Grassy => matches!(area, None | Some(AreaKind::Farmland)),
        Ground::Wood => area == Some(AreaKind::Wood),
        Ground::Sand => area == Some(AreaKind::Sand),
        Ground::Paving => area == Some(AreaKind::Paving),
        Ground::Water => area == Some(AreaKind::Water),
        Ground::Shore => {
            area != Some(AreaKind::Water)
                && ((land.taken)(x, z)
                    || (0..8).any(|k| {
                        let a = k as f32 * std::f32::consts::FRAC_PI_4;
                        wet(x + 9.0 * a.cos(), z + 9.0 * a.sin())
                    }))
        }
        Ground::Dry => area != Some(AreaKind::Water),
    };
    if !fits {
        return None;
    }
    // Off a stream, unless it is the stream's bank it wants.
    if rule.on != Ground::Shore && (land.taken)(x, z) {
        return None;
    }
    if out.claimed(x, z) {
        return None;
    }
    let size = rule.size[0] + (rule.size[1] - rule.size[0]) * biome::unit(g.rotate_left(29));
    if rule.claims_m > 0.0 {
        // All of it clear of the road, and of ground already claimed.
        let r = rule.claims_m;
        let ring = (0..8).map(|k| {
            let a = k as f32 * std::f32::consts::FRAC_PI_4;
            (x + r * a.cos(), z + r * a.sin())
        });
        for (px, pz) in ring.clone() {
            if (land.from_road)(px, pz) < CLAIM_CLEAR_M || out.claimed(px, pz) || wet(px, pz) {
                return None;
            }
        }
        let n = (r / CLAIM_CELL_M).ceil() as i64;
        let (ci, cj) = claim_cell(x, z);
        for di in -n..=n {
            for dj in -n..=n {
                out.claimed.insert((ci + di, cj + dj));
            }
        }
    }
    let y = (land.ground)(x, z)?;
    let yaw = if rule.face_road {
        (land.toward_road)(x, z)
    } else {
        biome::unit(g.rotate_left(37)) * std::f32::consts::TAU
    };
    let colour = if rule.colours.is_empty() {
        def.palette.tree(g.rotate_left(43))
    } else {
        let name = &rule.colours[(g.rotate_left(43) % rule.colours.len() as u64) as usize];
        def.palette.named(name).unwrap_or(def.palette.tree_a)
    };
    Some(Prop {
        shape: rule.shape,
        pos: [x, y, z],
        size,
        yaw,
        colour,
        motion: rule.motion.motion(),
        phase: biome::unit(g.rotate_left(53)) * std::f32::consts::TAU,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(json: &str) -> PropRule {
        serde_json::from_str(json).expect("a rule")
    }

    #[test]
    fn should_read_a_rule_and_refuse_nonsense() {
        let r = rule(
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 10, "to_m": 400, "on": "open", "size": [3, 5]}"#,
        );
        assert!(r.is_sane());
        assert_eq!(r.cluster, [1, 1]);
        assert_eq!(r.motion, Moves::Still);
        for bad in [
            // Both ways of placing, or neither.
            r#"{"shape": "cactus", "per_hectare": 2, "every_m": 100, "from_m": 10, "to_m": 40, "on": "open", "size": [3, 5]}"#,
            r#"{"shape": "cactus", "from_m": 10, "to_m": 40, "on": "open", "size": [3, 5]}"#,
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 50, "to_m": 40, "on": "open", "size": [3, 5]}"#,
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 10, "to_m": 4000, "on": "open", "size": [3, 5]}"#,
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 10, "to_m": 40, "on": "open", "size": [5, 3]}"#,
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 10, "to_m": 40, "on": "open", "size": [0, 3]}"#,
            r#"{"shape": "cactus", "per_hectare": 2, "from_m": 10, "to_m": 40, "on": "open", "size": [1, 3], "cluster": [0, 3]}"#,
            r#"{"shape": "cactus", "every_m": 2, "from_m": 10, "to_m": 40, "on": "open", "size": [1, 3]}"#,
            r#"{"shape": "cactus", "every_m": 200, "chance": 1.5, "from_m": 10, "to_m": 40, "on": "open", "size": [1, 3]}"#,
        ] {
            assert!(!rule(bad).is_sane(), "{bad}");
        }
        assert!(serde_json::from_str::<PropRule>(
            r#"{"shape": "dragon", "per_hectare": 2, "from_m": 10, "to_m": 40, "on": "open", "size": [1, 3]}"#
        )
        .is_err());
    }
}
