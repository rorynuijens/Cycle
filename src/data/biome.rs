//! Biomes: the look of the land beside the road — forest, farmland, coast,
//! mountains — loosely after the real place (the stylized-route skill, §7).
//!
//! A biome changes only what is off the road: ground colours, trees, the
//! haze. It never touches the road, its height or anything the ride is
//! driven from. Every biome is an entry in `biomes.json`; adding one is a new
//! entry plus, at most, new prop meshes.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::data::palette::{self, Palette, Rgb};
use crate::data::world::{splitmix, AreaKind};

/// The biomes, in the order their weights are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Biome {
    Temperate,
    Forest,
    Farmland,
    Polder,
    Mediterranean,
    Desert,
    Coast,
    Mountain,
    Wetland,
    Urban,
}

pub const COUNT: usize = 10;

impl Biome {
    pub const ALL: [Biome; COUNT] = [
        Biome::Temperate,
        Biome::Forest,
        Biome::Farmland,
        Biome::Polder,
        Biome::Mediterranean,
        Biome::Desert,
        Biome::Coast,
        Biome::Mountain,
        Biome::Wetland,
        Biome::Urban,
    ];

    /// This biome's look, from the bundled data file.
    pub fn def(self) -> &'static BiomeDef {
        &defs()[self as usize]
    }
}

/// How much each biome holds at one place, indexed by `Biome as usize`.
pub type Weights = [f32; COUNT];

/// How trees grow in a biome.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct TreeRules {
    /// Closest and furthest spacing between trees in a wood, in metres.
    pub forest_spacing_m: [f32; 2],
    /// Lone trees in open country, per hectare.
    pub open_per_hectare: f32,
    /// Share of trees that are cones rather than round crowns, 0–1.
    pub cone_share: f32,
}

/// One biome's look, resolved: the base palette with its overrides applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiomeDef {
    pub palette: Palette,
    /// Exponential haze, per metre — see [`haze`].
    pub fog_density: f32,
    pub trees: TreeRules,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDef {
    palette: std::collections::BTreeMap<String, String>,
    fog_density: f32,
    trees: TreeRules,
}

/// The haze starts this far from the eye; nearer, the scene is clear.
pub const FOG_START_M: f32 = 250.0;

/// How much of a colour the haze has taken at `distance_m`, 0–1, for a haze
/// of `density` per metre: squared exponential, clear near the rider and
/// closing in fast towards the edge of the world. The view's shaders work
/// the same sum; this is it where a test can check the densities.
#[cfg(test)]
pub fn haze(distance_m: f32, density: f32) -> f32 {
    let d = (distance_m - FOG_START_M).max(0.0) * density;
    1.0 - (-d * d).exp()
}

/// Read the biome data file. Every biome must be there, once; every colour
/// must name a palette entry a biome may change, in `#rrggbb`.
fn parse(text: &str) -> Result<[BiomeDef; COUNT], String> {
    let raw: std::collections::HashMap<Biome, RawDef> =
        serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(COUNT);
    for biome in Biome::ALL {
        let def = raw
            .get(&biome)
            .ok_or_else(|| format!("{biome:?} is missing"))?;
        let mut p = palette::BASE;
        for (name, value) in &def.palette {
            let rgb = palette::parse(value)
                .ok_or_else(|| format!("{biome:?}: {name} = {value:?} is not #rrggbb"))?;
            p = p
                .with(name, rgb)
                .ok_or_else(|| format!("{biome:?}: no colour {name} a biome may set"))?;
        }
        let spacing = def.trees.forest_spacing_m;
        let in_range = def.fog_density.is_finite()
            && def.fog_density > 0.0
            && spacing[0] > 0.0
            && spacing[0] <= spacing[1]
            && (0.0..=1.0).contains(&def.trees.cone_share)
            && def.trees.open_per_hectare.is_finite()
            && def.trees.open_per_hectare >= 0.0;
        if !in_range {
            return Err(format!("{biome:?}: a number is out of range"));
        }
        out.push(BiomeDef {
            palette: p,
            fog_density: def.fog_density,
            trees: def.trees,
        });
    }
    out.try_into().map_err(|_| "wrong count".to_owned())
}

fn defs() -> &'static [BiomeDef; COUNT] {
    static DEFS: OnceLock<[BiomeDef; COUNT]> = OnceLock::new();
    DEFS.get_or_init(|| {
        // The file is compiled in, and `should_read_the_bundled_biomes` reads
        // this very text: a build whose file fails here fails its tests.
        parse(include_str!("biomes.json")).expect("biomes.json is bundled and tested")
    })
}

/// A seed for anything placed at a scene position, from its whole-metre
/// cell and a `salt` naming what is being placed — the same place always
/// gets the same choice (stylized-route skill, rule 6).
pub fn seed_at(x: f32, z: f32, cell_m: f32, salt: u64) -> u64 {
    let i = (x / cell_m).floor() as i64 as u64;
    let j = (z / cell_m).floor() as i64 as u64;
    splitmix(splitmix(i ^ salt).wrapping_add(j))
}

/// A seed as a number in `[0, 1)`.
pub fn unit(seed: u64) -> f32 {
    (seed >> 40) as f32 / (1u64 << 24) as f32
}

/// Where snow lies year round, in metres: about 3000 m at 40° of latitude,
/// 2000 m at 55°.
pub fn snow_line_m(lat: f64) -> f32 {
    (3000.0 - (lat.abs() as f32 - 40.0) * (1000.0 / 15.0)).clamp(500.0, 5000.0)
}

/// How strongly the map's own word counts against the climate's guess.
const MAP_VOTE: f32 = 1.0;
const CLIMATE_VOTE: f32 = 0.35;

/// What one place votes for, from what grows there and how high it is.
/// `fallback` is the climate's guess. Fields are fields in any biome, so they
/// cast no vote of their own.
pub fn votes(area: Option<AreaKind>, raw_m: Option<f32>, fallback: Biome) -> Weights {
    let mut w = [0.0; COUNT];
    w[fallback as usize] += CLIMATE_VOTE;
    if area == Some(AreaKind::Wood) {
        w[Biome::Forest as usize] += MAP_VOTE;
    }
    if let Some(h) = raw_m {
        // High ground leans to mountain, whatever grows on it.
        w[Biome::Mountain as usize] += 1.5 * smoothstep(600.0, 1_500.0, h);
    }
    w
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The weights over the whole scene, on a coarse grid, blurred so biomes
/// never change along a hard line.
#[derive(Debug, Clone, PartialEq)]
pub struct BiomeMap {
    x0: f32,
    z0: f32,
    cell: f32,
    cols: usize,
    rows: usize,
    weights: Vec<Weights>,
}

/// Grid cell of the weight map, and the blur over it, in metres.
pub const MAP_CELL_M: f32 = 32.0;
pub const BLUR_M: f32 = 150.0;
/// The sea leans the land this near it to coast.
pub const COAST_REACH_M: f32 = 400.0;

impl BiomeMap {
    /// The map over `x_lo..x_hi`, `z_lo..z_hi`, from what each cell votes and
    /// whether it is sea, read at the cell's centre.
    pub fn build(
        (x_lo, x_hi): (f32, f32),
        (z_lo, z_hi): (f32, f32),
        sample: impl Fn(f32, f32) -> (Weights, bool),
    ) -> Self {
        let cell = MAP_CELL_M;
        let span = |lo: f32, hi: f32| {
            if hi >= lo && (hi - lo).is_finite() {
                ((hi - lo) / cell).ceil() as usize + 1
            } else {
                1
            }
        };
        let (cols, rows) = (span(x_lo, x_hi), span(z_lo, z_hi));
        let mut weights = Vec::with_capacity(cols * rows);
        let mut sea = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                let (w, s) = sample(x_lo + c as f32 * cell, z_lo + r as f32 * cell);
                weights.push(w);
                sea.push(if s { 1.0 } else { 0.0 });
            }
        }
        let radius = (BLUR_M / cell).round() as usize;
        let mut weights = blur(&weights, cols, rows, radius);
        let near_sea = blur(&sea, cols, rows, (COAST_REACH_M / cell).round() as usize);
        for (w, s) in weights.iter_mut().zip(&near_sea) {
            w[Biome::Coast as usize] += (s * 4.0).min(1.0);
        }
        Self {
            x0: x_lo,
            z0: z_lo,
            cell,
            cols,
            rows,
            weights,
        }
    }

    /// One biome everywhere.
    #[cfg(test)]
    pub fn uniform(biome: Biome) -> Self {
        let mut w = [0.0; COUNT];
        w[biome as usize] = 1.0;
        Self {
            x0: 0.0,
            z0: 0.0,
            cell: MAP_CELL_M,
            cols: 1,
            rows: 1,
            weights: vec![w],
        }
    }

    /// The weights at `(x, z)`, bilinear between the grid's cells; the
    /// nearest edge cell's beyond the grid.
    pub fn weights_at(&self, x: f32, z: f32) -> Weights {
        let fx = ((x - self.x0) / self.cell).clamp(0.0, (self.cols - 1) as f32);
        let fz = ((z - self.z0) / self.cell).clamp(0.0, (self.rows - 1) as f32);
        let (fx, fz) = (
            if fx.is_finite() { fx } else { 0.0 },
            if fz.is_finite() { fz } else { 0.0 },
        );
        let (c0, r0) = (fx.floor() as usize, fz.floor() as usize);
        let (c1, r1) = ((c0 + 1).min(self.cols - 1), (r0 + 1).min(self.rows - 1));
        let (tx, tz) = (fx - c0 as f32, fz - r0 as f32);
        let at = |r: usize, c: usize| &self.weights[r * self.cols + c];
        let mut out = [0.0; COUNT];
        for (b, o) in out.iter_mut().enumerate() {
            let top = at(r0, c0)[b] + (at(r0, c1)[b] - at(r0, c0)[b]) * tx;
            let bottom = at(r1, c0)[b] + (at(r1, c1)[b] - at(r1, c0)[b]) * tx;
            *o = top + (bottom - top) * tz;
        }
        out
    }

    /// The one biome that colours the ground at `(x, z)`. Colours are never
    /// averaged between biomes — that turns them to mud — so where two meet,
    /// each place picks one, with a little seeded noise on the weights so the
    /// border scatters instead of running in a line.
    pub fn dominant_at(&self, x: f32, z: f32) -> Biome {
        dominant(&self.weights_at(x, z), seed_at(x, z, 12.0, 0xB10E))
    }
}

/// The biome with the most weight, after a nudge of up to ±0.15 each from
/// `seed`.
pub fn dominant(w: &Weights, seed: u64) -> Biome {
    let mut best = (Biome::Temperate, f32::MIN);
    for b in Biome::ALL {
        let nudge = (unit(splitmix(seed ^ b as u64)) - 0.5) * 0.3;
        let v = w[b as usize] + nudge;
        if v > best.1 {
            best = (b, v);
        }
    }
    best.0
}

/// The sky and haze for the weights at the rider: blended, unlike the
/// ground — a sky has no border to scatter.
pub fn sky(w: &Weights) -> (Rgb, Rgb, f32) {
    let total: f32 = w.iter().map(|v| v.max(0.0)).sum();
    if total.is_nan() || total <= 0.0 {
        let p = Biome::Temperate.def();
        return (p.palette.sky_top, p.palette.sky_horizon, p.fog_density);
    }
    let mut top = [0.0; 3];
    let mut horizon = [0.0; 3];
    let mut fog = 0.0;
    for b in Biome::ALL {
        let k = w[b as usize].max(0.0) / total;
        let d = b.def();
        for i in 0..3 {
            top[i] += d.palette.sky_top[i] * k;
            horizon[i] += d.palette.sky_horizon[i] * k;
        }
        fog += d.fog_density * k;
    }
    (top, horizon, fog)
}

/// Box blur of radius `r` cells, along rows then columns; at the grid's edge
/// each cell averages what there is.
fn blur<T: Copy + Default + Blend>(grid: &[T], cols: usize, rows: usize, r: usize) -> Vec<T> {
    if r == 0 || grid.is_empty() {
        return grid.to_vec();
    }
    let pass = |src: &[T], len: usize, count: usize, at: &dyn Fn(usize, usize) -> usize| {
        let mut out = src.to_vec();
        for line in 0..count {
            // Running sums along the line.
            let mut prefix = vec![T::default(); len + 1];
            for k in 0..len {
                prefix[k + 1] = prefix[k].add(&src[at(line, k)]);
            }
            for k in 0..len {
                let lo = k.saturating_sub(r);
                let hi = (k + r).min(len - 1);
                let n = (hi - lo + 1) as f32;
                out[at(line, k)] = prefix[hi + 1].sub(&prefix[lo]).scale(1.0 / n);
            }
        }
        out
    };
    let along_rows = pass(grid, cols, rows, &|row, k| row * cols + k);
    pass(&along_rows, rows, cols, &|col, k| k * cols + col)
}

/// What [`blur`] needs of a cell.
trait Blend: Sized {
    fn add(&self, o: &Self) -> Self;
    fn sub(&self, o: &Self) -> Self;
    fn scale(&self, k: f32) -> Self;
}

impl Blend for f32 {
    fn add(&self, o: &Self) -> Self {
        self + o
    }
    fn sub(&self, o: &Self) -> Self {
        self - o
    }
    fn scale(&self, k: f32) -> Self {
        self * k
    }
}

impl Blend for Weights {
    fn add(&self, o: &Self) -> Self {
        std::array::from_fn(|i| self[i] + o[i])
    }
    fn sub(&self, o: &Self) -> Self {
        std::array::from_fn(|i| self[i] - o[i])
    }
    fn scale(&self, k: f32) -> Self {
        std::array::from_fn(|i| self[i] * k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_the_bundled_biomes() {
        let defs = parse(include_str!("biomes.json")).expect("the bundled file");
        assert_eq!(
            defs[Biome::Forest as usize].palette.grass_low,
            palette::hex(0x5FAF48)
        );
        assert_eq!(defs[Biome::Temperate as usize].palette, palette::BASE);
    }

    #[test]
    fn should_keep_the_track_the_same_in_every_biome() {
        for b in Biome::ALL {
            let p = b.def().palette;
            assert_eq!(
                (p.road, p.road_edge, p.road_line, p.outline),
                (
                    palette::BASE.road,
                    palette::BASE.road_edge,
                    palette::BASE.road_line,
                    palette::BASE.outline
                ),
                "{b:?}"
            );
        }
    }

    #[test]
    fn should_refuse_a_file_that_misses_a_biome_or_names_a_wrong_colour() {
        let good: serde_json::Value =
            serde_json::from_str(include_str!("biomes.json")).expect("json");
        let mut missing = good.clone();
        missing.as_object_mut().expect("object").remove("desert");
        assert!(parse(&missing.to_string()).unwrap_err().contains("Desert"));

        let mut road = good.clone();
        road["coast"]["palette"]["road"] = "#FF0000".into();
        assert!(
            parse(&road.to_string()).is_err(),
            "a biome repainted the road"
        );

        let mut typo = good.clone();
        typo["coast"]["palette"]["gras_low"] = "#FF0000".into();
        assert!(parse(&typo.to_string()).is_err());

        let mut short = good.clone();
        short["coast"]["palette"]["rock"] = "#FFF".into();
        assert!(parse(&short.to_string()).is_err());

        let mut fog = good;
        fog["coast"]["fog_density"] = (-1.0).into();
        assert!(parse(&fog.to_string()).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn should_hide_the_edge_of_the_world_in_every_biome() {
        // The world reaches 3.2 km from the road; by 3.1 km the haze must
        // have swallowed it, whichever biome the rider is in.
        for b in Biome::ALL {
            let h = haze(3_100.0, b.def().fog_density);
            assert!(h >= 0.98, "{b:?} haze at the edge is only {h}");
        }
    }

    #[test]
    fn should_leave_the_near_road_clear() {
        for b in Biome::ALL {
            assert_eq!(haze(FOG_START_M, b.def().fog_density), 0.0);
            assert_eq!(haze(0.0, b.def().fog_density), 0.0);
            assert!(haze(400.0, b.def().fog_density) < 0.05, "{b:?}");
        }
    }

    #[test]
    fn should_put_the_snow_line_where_the_skill_says() {
        assert_eq!(snow_line_m(40.0), 3000.0);
        assert!((snow_line_m(55.0) - 2000.0).abs() < 1e-3);
        assert_eq!(snow_line_m(-40.0), 3000.0);
    }

    #[test]
    fn should_let_the_map_outvote_the_climate() {
        let w = votes(Some(AreaKind::Wood), Some(30.0), Biome::Mediterranean);
        assert_eq!(dominant(&w, 0), Biome::Forest);
        assert_eq!(dominant(&w, u64::MAX), Biome::Forest);
        let w = votes(None, Some(30.0), Biome::Mediterranean);
        assert_eq!(dominant(&w, 7), Biome::Mediterranean);
    }

    #[test]
    fn should_lean_high_ground_to_mountain() {
        let w = votes(None, Some(2_000.0), Biome::Temperate);
        assert_eq!(dominant(&w, 3), Biome::Mountain);
        let w = votes(None, Some(f32::NAN), Biome::Temperate);
        assert_eq!(dominant(&w, 3), Biome::Temperate);
    }

    /// A wood west of x = 0, open country east of it.
    fn split_map() -> BiomeMap {
        BiomeMap::build((-2_000.0, 2_000.0), (-500.0, 500.0), |x, _| {
            let area = (x < 0.0).then_some(AreaKind::Wood);
            (votes(area, Some(20.0), Biome::Temperate), false)
        })
    }

    #[test]
    fn should_keep_each_side_of_a_border_its_own_biome() {
        let m = split_map();
        assert_eq!(m.dominant_at(-800.0, 0.0), Biome::Forest);
        assert_eq!(m.dominant_at(800.0, 0.0), Biome::Temperate);
    }

    #[test]
    fn should_blend_weights_across_a_border_not_step_them() {
        // Far from the border each side is pure; across it the forest weight
        // falls over the blur's width, never in one jump.
        let m = split_map();
        let forest = |x: f32| m.weights_at(x, 0.0)[Biome::Forest as usize];
        assert!((forest(-1_000.0) - 1.0).abs() < 1e-4);
        assert!(forest(1_000.0).abs() < 1e-4);
        let mut last = forest(-400.0);
        let mut x = -400.0;
        while x < 400.0 {
            x += 8.0;
            let now = forest(x);
            assert!(now <= last + 1e-4, "rose at {x}");
            assert!(last - now < 0.05, "stepped {} at {x}", last - now);
            last = now;
        }
    }

    #[test]
    fn should_scatter_the_border_instead_of_drawing_a_line() {
        // Along the border itself both biomes appear, mixed.
        let m = split_map();
        let picks: Vec<Biome> = (0..200)
            .map(|k| m.dominant_at(0.0, -400.0 + k as f32 * 4.0))
            .collect();
        assert!(picks.contains(&Biome::Forest) && picks.contains(&Biome::Temperate));
    }

    #[test]
    fn should_lean_land_beside_the_sea_to_coast() {
        let m = BiomeMap::build((-2_000.0, 2_000.0), (-200.0, 200.0), |x, _| {
            (votes(None, Some(5.0), Biome::Temperate), x < 0.0)
        });
        assert_eq!(m.dominant_at(150.0, 0.0), Biome::Coast);
        assert_eq!(m.dominant_at(1_500.0, 0.0), Biome::Temperate);
    }

    #[test]
    fn should_answer_anywhere_even_off_the_map_or_at_nan() {
        let m = split_map();
        for (x, z) in [
            (-1e9, 0.0),
            (1e9, 1e9),
            (f32::NAN, 0.0),
            (0.0, f32::INFINITY),
        ] {
            let w = m.weights_at(x, z);
            assert!(w.iter().all(|v| v.is_finite()), "{x}, {z}");
        }
        // A degenerate box still makes a map.
        let tiny = BiomeMap::build((5.0, 5.0), (f32::NAN, 0.0), |_, _| ([0.0; COUNT], false));
        let _ = tiny.dominant_at(0.0, 0.0);
    }

    #[test]
    fn should_seed_the_same_place_the_same_way() {
        assert_eq!(seed_at(10.3, -4.0, 1.0, 9), seed_at(10.9, -3.2, 1.0, 9));
        assert_ne!(seed_at(10.3, -4.0, 1.0, 9), seed_at(11.3, -4.0, 1.0, 9));
        assert_ne!(seed_at(10.3, -4.0, 1.0, 9), seed_at(10.3, -4.0, 1.0, 8));
        for s in [0, 1, u64::MAX, 0xDEAD_BEEF] {
            assert!((0.0..1.0).contains(&unit(s)));
        }
    }

    #[test]
    fn should_blend_the_sky_and_survive_no_weight_at_all() {
        let mut w = [0.0; COUNT];
        w[Biome::Desert as usize] = 1.0;
        let (_, horizon, fog) = sky(&w);
        assert_eq!(horizon, Biome::Desert.def().palette.sky_horizon);
        assert_eq!(fog, Biome::Desert.def().fog_density);
        let (_, horizon, _) = sky(&[0.0; COUNT]);
        assert_eq!(horizon, palette::BASE.sky_horizon);
        let (_, horizon, _) = sky(&[f32::NAN; COUNT]);
        assert!(horizon.iter().all(|v| v.is_finite()));
    }
}
