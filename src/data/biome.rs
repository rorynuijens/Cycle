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

use crate::data::dressing::PropRule;
use crate::data::palette::{self, Palette, Rgb};
use crate::data::props::Shape;
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
    /// Share of trees that are the second of [`TreeRules::kinds`] rather
    /// than the first, 0–1.
    pub cone_share: f32,
    /// The two kinds of tree that grow: a round crown and a conifer in the
    /// countryside, olives and cypresses by the Mediterranean.
    pub kinds: [Shape; 2],
}

/// The herd that grazes beside the road, and the birds overhead.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnimalRules {
    /// What grazes in herds; `None` for nothing.
    pub herd: Option<Shape>,
    /// Palette names a herd's coats are picked from.
    pub coats: Vec<String>,
    /// How long one is, in metres, drawn a little larger than life.
    pub herd_size_m: f32,
    /// Share of the places a herd could graze that have one, and how many.
    pub herd_chance: f32,
    pub herd_count: [u32; 2],
    /// Whether the herd grazes among the trees, as deer do.
    pub herd_in_woods: bool,
    /// Birds circling overhead, and how big — in the palette's `bird`.
    pub birds: bool,
    pub bird_size_m: f32,
}

/// The shape of a biome's land (the stylized-route skill, §7.4). Every one
/// is worked before the land is fitted to the road, so the road always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relief {
    /// Low hills by the road, bigger ones further off.
    Rolling,
    /// Dead level to the horizon.
    Flat,
    /// Sand dunes in ridges along the wind, and flat-topped mesas far off.
    Dunes,
    /// Hillsides cut into steps.
    Terraces,
    /// Sharp ridges and high peaks away from the road.
    Peaks,
}

/// The open water a biome has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaterKind {
    None,
    /// Brooks wandering alongside the road.
    Streams,
    /// Straight ditches between fields.
    Ditches,
    /// Pools between low mounds.
    Pools,
    /// The sea on one side, behind a beach.
    Sea,
    /// Now and then a pool in the sand.
    Oasis,
}

/// How a biome's land lies and what covers it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandRules {
    pub relief: Relief,
    /// How high its hills are, against the countryside's.
    pub relief_scale: f32,
    /// More woods (positive) or fewer (negative): shifts where woods start,
    /// on noise that runs about −0.6 to 0.6.
    pub wood: f32,
    /// Woods keep at least this far from the road's line, in metres.
    pub wood_from_m: f32,
    /// Fields cover the land where its noise is above this: −1 everywhere,
    /// 1 nowhere.
    pub fields: f32,
    /// Share of fields under a crop rather than grass, 0–1.
    pub crops: f32,
    /// Ground within this far of the road's line is paved, in metres.
    pub paving_m: f32,
    pub water: WaterKind,
    /// Rows of poplars along the road.
    pub poplar_rows: bool,
}

/// One biome's look, resolved: the base palette with its overrides applied.
#[derive(Debug, Clone, PartialEq)]
pub struct BiomeDef {
    pub palette: Palette,
    /// Exponential haze, per metre — see [`haze`].
    pub fog_density: f32,
    pub trees: TreeRules,
    pub land: LandRules,
    pub animals: AnimalRules,
    /// Everything else it sets about its land.
    pub props: Vec<PropRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDef {
    palette: std::collections::BTreeMap<String, String>,
    fog_density: f32,
    trees: TreeRules,
    land: LandRules,
    animals: AnimalRules,
    #[serde(default)]
    props: Vec<PropRule>,
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
            && def.trees.open_per_hectare >= 0.0
            && (0.0..=4.0).contains(&def.land.relief_scale)
            && (-1.0..=1.0).contains(&def.land.wood)
            && (0.0..=1_000.0).contains(&def.land.wood_from_m)
            && (-1.0..=1.0).contains(&def.land.fields)
            && (0.0..=1.0).contains(&def.land.crops)
            && (0.0..=500.0).contains(&def.land.paving_m);
        let a = &def.animals;
        let animals_sane = a.herd_size_m.is_finite()
            && a.herd_size_m > 0.0
            && (0.0..=1.0).contains(&a.herd_chance)
            && (1..=a.herd_count[1].max(1)).contains(&a.herd_count[0])
            && a.herd_count[1] <= 20
            && a.bird_size_m.is_finite()
            && a.bird_size_m > 0.0
            && !a.coats.is_empty()
            && a.coats.iter().all(|n| p.named(n).is_some());
        if !in_range || !animals_sane {
            return Err(format!("{biome:?}: a number is out of range"));
        }
        if let Some(bad) = def.props.iter().find(|r| !r.is_sane()) {
            return Err(format!(
                "{biome:?}: the {:?} rule makes no sense",
                bad.shape
            ));
        }
        if let Some(bad) = def
            .props
            .iter()
            .flat_map(|r| &r.colours)
            .find(|n| p.named(n).is_none())
        {
            return Err(format!("{biome:?}: no colour {bad}"));
        }
        out.push(BiomeDef {
            palette: p,
            fog_density: def.fog_density,
            trees: def.trees,
            land: def.land,
            animals: def.animals.clone(),
            props: def.props.clone(),
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

/// What the world around a route is made of. The rider picks it per route,
/// where the route is picked to ride; it changes only what is off the road.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Scenery {
    /// Meadows and fields, woods leaning to forest, high ground to mountain.
    #[default]
    Countryside,
    /// One biome for the whole ride.
    Only(Biome),
    /// Biomes that change along the ride — see [`mix_segments`].
    Mixed,
}

impl Scenery {
    /// Every choice, in the order the picker lists them.
    pub const CHOICES: [Scenery; 11] = [
        Scenery::Countryside,
        Scenery::Only(Biome::Forest),
        Scenery::Only(Biome::Farmland),
        Scenery::Only(Biome::Polder),
        Scenery::Only(Biome::Mediterranean),
        Scenery::Only(Biome::Desert),
        Scenery::Only(Biome::Coast),
        Scenery::Only(Biome::Mountain),
        Scenery::Only(Biome::Wetland),
        Scenery::Only(Biome::Urban),
        Scenery::Mixed,
    ];

    /// The name it is stored under.
    pub fn key(self) -> &'static str {
        match self {
            Scenery::Countryside => "countryside",
            Scenery::Mixed => "mixed",
            Scenery::Only(b) => b.key(),
        }
    }

    /// The choice stored as `key`; `None` for anything that is not one of
    /// [`Scenery::CHOICES`] — the database is read, not trusted.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::CHOICES.into_iter().find(|s| s.key() == key)
    }

    /// The name the rider sees.
    pub fn label(self) -> &'static str {
        match self {
            Scenery::Countryside => "Countryside",
            Scenery::Mixed => "Mixed",
            Scenery::Only(b) => match b {
                Biome::Temperate => "Temperate",
                Biome::Forest => "Forest",
                Biome::Farmland => "Farmland",
                Biome::Polder => "Polder",
                Biome::Mediterranean => "Mediterranean",
                Biome::Desert => "Desert",
                Biome::Coast => "Coast",
                Biome::Mountain => "Mountain",
                Biome::Wetland => "Wetland",
                Biome::Urban => "Urban",
            },
        }
    }
}

impl Biome {
    /// The biome's name in `biomes.json`.
    pub fn key(self) -> &'static str {
        match self {
            Biome::Temperate => "temperate",
            Biome::Forest => "forest",
            Biome::Farmland => "farmland",
            Biome::Polder => "polder",
            Biome::Mediterranean => "mediterranean",
            Biome::Desert => "desert",
            Biome::Coast => "coast",
            Biome::Mountain => "mountain",
            Biome::Wetland => "wetland",
            Biome::Urban => "urban",
        }
    }
}

/// A mixed ride's stretches are this long, before the last is stretched to
/// the finish (the stylized-route skill, §7.7).
pub const MIX_MIN_M: f32 = 5_000.0;
pub const MIX_MAX_M: f32 = 15_000.0;
/// Two stretches blend over this much road, centred on their border.
pub const MIX_BLEND_M: f32 = 400.0;

/// The biomes of a mixed ride `length_m` long: where each stretch starts,
/// and its biome. Never the same biome twice running; a remainder too short
/// to be a stretch of its own goes to the one before it. Seeded, so a route
/// always mixes the same way.
pub fn mix_segments(seed: u64, length_m: f32) -> Vec<(f32, Biome)> {
    let length = if length_m.is_finite() { length_m } else { 0.0 };
    let pick = |k: u64, not: Option<Biome>| {
        let pool: Vec<Biome> = Biome::ALL.into_iter().filter(|b| Some(*b) != not).collect();
        pool[(splitmix(seed ^ 0x313C ^ k.wrapping_mul(0x9E37)) % pool.len() as u64) as usize]
    };
    let mut out = vec![(0.0, pick(0, None))];
    let mut start = 0.0;
    for k in 1u64.. {
        let len = MIX_MIN_M + (MIX_MAX_M - MIX_MIN_M) * unit(splitmix(seed ^ 0x51E6 ^ k));
        start += len;
        if length - start < MIX_MIN_M {
            break;
        }
        let prev = out.last().map(|s| s.1);
        out.push((start, pick(k, prev)));
    }
    out
}

/// The weights `along_m` metres into a mixed ride of `segments`: its
/// stretch's biome alone, or two of them across a border.
pub fn mix_weights(segments: &[(f32, Biome)], along_m: f32) -> Weights {
    let mut w = [0.0; COUNT];
    let Some(first) = segments.first() else {
        w[Biome::Temperate as usize] = 1.0;
        return w;
    };
    let along = if along_m.is_finite() { along_m } else { 0.0 };
    let i = segments.partition_point(|s| s.0 <= along).max(1) - 1;
    let here = segments.get(i).unwrap_or(first).1;
    let half = MIX_BLEND_M / 2.0;
    // Into the next stretch, or still coming out of the last one.
    let (a, b, t) = match (
        segments.get(i + 1),
        i.checked_sub(1).and_then(|p| segments.get(p)),
    ) {
        (Some(next), _) if along > next.0 - half => {
            (here, next.1, (along - (next.0 - half)) / MIX_BLEND_M)
        }
        (_, Some(prev)) if along < segments[i].0 + half => {
            (prev.1, here, (along - (segments[i].0 - half)) / MIX_BLEND_M)
        }
        _ => (here, here, 0.0),
    };
    let t = t.clamp(0.0, 1.0);
    w[a as usize] += 1.0 - t;
    w[b as usize] += t;
    w
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

    #[test]
    fn should_store_every_scenery_under_its_own_key_and_read_it_back() {
        let mut keys: Vec<&str> = Scenery::CHOICES.iter().map(|s| s.key()).collect();
        for s in Scenery::CHOICES {
            assert_eq!(Scenery::from_key(s.key()), Some(s), "{s:?}");
        }
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            Scenery::CHOICES.len(),
            "two choices share a key"
        );
        // Every biome but the countryside's own base can be picked alone.
        for b in Biome::ALL {
            let listed = Scenery::CHOICES.contains(&Scenery::Only(b));
            assert_eq!(listed, b != Biome::Temperate, "{b:?}");
        }
    }

    #[test]
    fn should_refuse_a_stored_scenery_it_does_not_know() {
        for bad in ["", "Desert", "desert ", "temperate", "mixed;", "\u{0}"] {
            assert_eq!(Scenery::from_key(bad), None, "{bad:?}");
        }
        assert_eq!(Scenery::default(), Scenery::Countryside);
    }

    #[test]
    fn should_mix_stretches_of_five_to_fifteen_km_never_twice_running() {
        for seed in 0..200u64 {
            let length = 1_000.0 + seed as f32 * 997.0;
            let segs = mix_segments(seed, length);
            assert_eq!(segs[0].0, 0.0);
            for pair in segs.windows(2) {
                let len = pair[1].0 - pair[0].0;
                assert!((MIX_MIN_M..=MIX_MAX_M).contains(&len), "{seed}: {len}");
                assert_ne!(pair[0].1, pair[1].1, "{seed}: same biome twice");
            }
            // The last stretch runs to the finish: never under 5 km unless the
            // whole ride is, never over the longest plus a shortest.
            let last = length - segs.last().map(|s| s.0).unwrap_or(0.0);
            if segs.len() > 1 {
                assert!(
                    (MIX_MIN_M..MIX_MAX_M + MIX_MIN_M).contains(&last),
                    "{seed}: {last}"
                );
            }
            assert_eq!(segs, mix_segments(seed, length), "not deterministic");
        }
    }

    #[test]
    fn should_give_a_short_ride_one_biome_and_survive_nonsense_lengths() {
        assert_eq!(mix_segments(4, 9_999.0).len(), 1);
        for bad in [f32::NAN, f32::INFINITY, -5.0, 0.0] {
            assert_eq!(mix_segments(4, bad).len(), 1, "{bad}");
        }
    }

    #[test]
    fn should_use_more_than_one_biome_over_many_long_rides() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..40u64 {
            seen.extend(mix_segments(seed, 120_000.0).into_iter().map(|s| s.1));
        }
        assert_eq!(seen.len(), COUNT, "the mix never picks some biomes");
    }

    #[test]
    fn should_blend_two_stretches_over_400_m_centred_on_their_border() {
        let segs = [
            (0.0, Biome::Desert),
            (6_000.0, Biome::Forest),
            (13_000.0, Biome::Coast),
        ];
        let at = |d: f32| mix_weights(&segs, d);
        let (desert, forest) = (Biome::Desert as usize, Biome::Forest as usize);
        assert_eq!(at(5_000.0)[desert], 1.0);
        assert_eq!(
            at(5_800.0)[desert],
            1.0,
            "blend starts 200 m before the border"
        );
        assert_eq!(at(6_000.0)[desert], 0.5);
        assert_eq!(at(6_000.0)[forest], 0.5);
        assert!((at(6_100.0)[forest] - 0.75).abs() < 1e-6);
        assert_eq!(at(6_200.0)[forest], 1.0, "and ends 200 m after it");
        assert_eq!(at(9_000.0)[forest], 1.0);
        assert_eq!(at(13_000.0)[Biome::Coast as usize], 0.5);
        assert_eq!(at(40_000.0)[Biome::Coast as usize], 1.0);
        for d in [-1e9, f32::NAN, f32::INFINITY, 0.0, 1e12] {
            let total: f32 = at(d).iter().sum();
            assert_eq!(total, 1.0, "{d}");
        }
        assert_eq!(mix_weights(&[], 10.0)[Biome::Temperate as usize], 1.0);
    }
}
