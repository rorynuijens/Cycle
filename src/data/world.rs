//! The made-up countryside around a route.
//!
//! Nothing here comes from a map or the network: the land is generated from
//! the route itself and a seed taken from it. The route stays the spine —
//! its line, distance and heights are the ride's (the stylized-route skill,
//! rules 1–5) — and everything around it is invented: rolling hills that
//! follow the road's own height, fields, woods and streams. The same GPX
//! always grows the same world (rule 6).
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::biome::{Biome, BiomeMap, LandRules, Relief, WaterKind};
use crate::data::course3d::RoadPoint;

/// What covers the land at a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AreaKind {
    Water,
    Wood,
    Farmland,
    /// A beach, or the sand round an oasis.
    Sand,
    /// Squares and pavements in town.
    Paving,
}

/// A well-mixed hash, for every seeded choice.
pub fn splitmix(id: u64) -> u64 {
    let mut z = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A hash as a number in `[0, 1)`.
fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Smooth noise in `[-1, 1]`: random values on a unit lattice, eased between.
fn value_noise(seed: u64, x: f32, z: f32) -> f32 {
    if !(x.is_finite() && z.is_finite()) {
        return 0.0;
    }
    let (xi, zi) = (x.floor(), z.floor());
    let (fx, fz) = (x - xi, z - zi);
    let (sx, sz) = (fx * fx * (3.0 - 2.0 * fx), fz * fz * (3.0 - 2.0 * fz));
    let at = |i: f32, j: f32| {
        let h = splitmix(splitmix(seed ^ (i as i64 as u64)).wrapping_add(j as i64 as u64));
        unit(h) * 2.0 - 1.0
    };
    let a = at(xi, zi);
    let b = at(xi + 1.0, zi);
    let c = at(xi, zi + 1.0);
    let d = at(xi + 1.0, zi + 1.0);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sz
}

/// Noise summed over `octaves`, each twice as fine and half as strong, at a
/// base feature size of `scale_m`. Stays within `[-1, 1]`.
pub fn fbm(seed: u64, x: f32, z: f32, scale_m: f32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut norm, mut f) = (0.0, 1.0, 0.0, 1.0 / scale_m);
    for o in 0..octaves {
        sum += amp * value_noise(seed.wrapping_add(u64::from(o) * 0x51ED), x * f, z * f);
        norm += amp;
        amp *= 0.5;
        f *= 2.0;
    }
    if norm > 0.0 {
        sum / norm
    } else {
        0.0
    }
}

/// Fields keep this far back from the road: a grass verge.
const VERGE_M: f32 = 22.0;

/// The sea starts this far from the road's line, its beach a little nearer,
/// and lies this far below the road beside it; the beach a little higher.
const SEA_FROM_M: f32 = 65.0;
const SEA_DROP_M: f32 = 2.0;
const BEACH_DROP_M: f32 = 1.2;
/// Pools and oases keep this far from the road's line, and lie this far
/// below the road beside them.
const POOL_FROM_M: f32 = 45.0;
const OASIS_FROM_M: f32 = 80.0;
const POOL_DROP_M: f32 = 0.7;
/// Ditches are laid in steps this long, and one runs away from the road
/// every this many metres of it.
const DITCH_STEP_M: f32 = 12.0;
const DITCH_ACROSS_EVERY_M: f32 = 90.0;

/// The land is planned on a coarse grid this fine, in metres.
const BASE_CELL_M: f32 = 100.0;
/// Road points this far apart along the route anchor the land's height.
const ANCHOR_EVERY_M: f32 = 250.0;
/// The world reaches this far from the road in every direction.
pub const REACH_M: f32 = 3_200.0;

/// Low hills near the road; higher, broader ones far from it.
const NEAR_HILLS_M: f32 = 4.0;
const FAR_HILLS_M: f32 = 70.0;
const HILLS_FROM_M: f32 = 150.0;
const HILLS_FULL_M: f32 = 2_200.0;

/// The countryside along one route.
#[derive(Debug, Clone, PartialEq)]
pub struct World {
    seed: u64,
    x0: f32,
    z0: f32,
    cols: usize,
    rows: usize,
    /// Per grid point: the road's height carried out over the land, and how
    /// far the nearest anchor of the road is.
    base: Vec<(f32, f32)>,
    /// Per grid point: how far along the route the nearest road is.
    along: Vec<f32>,
    /// Per grid point: whether the nearest road passes it on its left
    /// (true) or its right, going the way of the ride.
    left: Vec<bool>,
    /// Which biome is where, once the look is decided; until then the
    /// land is the countryside's.
    biomes: Option<BiomeMap>,
}

impl World {
    /// The world around a route whose centreline is `road`.
    pub fn new(road: &[RoadPoint]) -> Self {
        let seed = road.iter().fold(0x7E11_A5EEu64, |h, p| {
            splitmix(h ^ u64::from(p.pos[0].to_bits()) ^ (u64::from(p.pos[2].to_bits()) << 32))
        });
        // Anchors: a point of road every so often, with its height.
        let mut anchors: Vec<[f32; 3]> = Vec::new();
        let mut anchor_d: Vec<f32> = Vec::new();
        let mut next = f32::MIN;
        for p in road {
            if p.distance_m >= next && p.pos.iter().all(|v| v.is_finite()) {
                anchors.push(p.pos);
                anchor_d.push(p.distance_m);
                next = p.distance_m + ANCHOR_EVERY_M;
            }
        }
        if let Some(last) = road.last().filter(|p| p.pos.iter().all(|v| v.is_finite())) {
            anchors.push(last.pos);
            anchor_d.push(last.distance_m);
        }
        let (mut x_lo, mut x_hi, mut z_lo, mut z_hi) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        if let Some(first) = anchors.first() {
            (x_lo, x_hi, z_lo, z_hi) = (first[0], first[0], first[2], first[2]);
        }
        for a in &anchors {
            x_lo = x_lo.min(a[0]);
            x_hi = x_hi.max(a[0]);
            z_lo = z_lo.min(a[2]);
            z_hi = z_hi.max(a[2]);
        }
        let (x0, z0) = (x_lo - REACH_M, z_lo - REACH_M);
        let cols = ((x_hi + REACH_M - x0) / BASE_CELL_M).ceil() as usize + 2;
        let rows = ((z_hi + REACH_M - z0) / BASE_CELL_M).ceil() as usize + 2;
        let mut base = Vec::with_capacity(cols * rows);
        let mut along = Vec::with_capacity(cols * rows);
        let mut left = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                let (x, z) = (x0 + c as f32 * BASE_CELL_M, z0 + r as f32 * BASE_CELL_M);
                // Inverse-distance weighting: the land takes the height of
                // the road nearest it, blending smoothly between stretches.
                // Distance to the road is to the line between anchors, not
                // to the anchors: they are 250 m apart, and a field's verge
                // is measured in metres.
                let (mut wsum, mut hsum, mut nearest) = (0.0f64, 0.0f64, f32::MAX);
                let mut nearest_along = 0.0;
                let mut nearest_left = false;
                for (k, a) in anchors.iter().enumerate() {
                    let d2 = (a[0] - x).powi(2) + (a[2] - z).powi(2);
                    let w = 1.0 / (f64::from(d2) + 2_500.0).powi(2);
                    wsum += w;
                    hsum += w * f64::from(a[1]);
                    let (seg_d2, t) = match anchors.get(k + 1) {
                        Some(b) => segment_d2_t((a[0], a[2]), (b[0], b[2]), x, z),
                        None => (d2, 0.0),
                    };
                    if seg_d2 < nearest {
                        nearest = seg_d2;
                        let (d0, d1) = (anchor_d[k], *anchor_d.get(k + 1).unwrap_or(&anchor_d[k]));
                        nearest_along = d0 + (d1 - d0) * t;
                        if let Some(b) = anchors.get(k + 1) {
                            // Right of travel is (−dz, dx): a place on that
                            // side has a positive cross product.
                            let cross = (b[0] - a[0]) * (z - a[2]) - (b[2] - a[2]) * (x - a[0]);
                            nearest_left = cross < 0.0;
                        }
                    }
                }
                along.push(nearest_along);
                left.push(nearest_left);
                let h = if wsum > 0.0 {
                    (hsum / wsum) as f32
                } else {
                    0.0
                };
                base.push((h, nearest.sqrt()));
            }
        }
        Self {
            seed,
            x0,
            z0,
            cols,
            rows,
            base,
            along,
            left,
            biomes: None,
        }
    }

    /// Shape and cover the land by `biomes` from now on.
    pub fn set_biomes(&mut self, biomes: BiomeMap) {
        self.biomes = Some(biomes);
    }

    /// The biome that decides what covers the land at `(x, z)`.
    pub fn biome_at(&self, x: f32, z: f32) -> Biome {
        self.biomes
            .as_ref()
            .map_or(Biome::Temperate, |m| m.dominant_at(x, z))
    }

    fn rules_at(&self, x: f32, z: f32) -> &'static LandRules {
        &self.biome_at(x, z).def().land
    }

    /// The world's seed: every choice in it starts from this.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Base height and distance to the road, bilinear on the grid.
    fn base_at(&self, x: f32, z: f32) -> (f32, f32) {
        let fx = ((x - self.x0) / BASE_CELL_M).clamp(0.0, (self.cols - 1) as f32);
        let fz = ((z - self.z0) / BASE_CELL_M).clamp(0.0, (self.rows - 1) as f32);
        let (fx, fz) = (
            if fx.is_finite() { fx } else { 0.0 },
            if fz.is_finite() { fz } else { 0.0 },
        );
        let (c0, r0) = (fx.floor() as usize, fz.floor() as usize);
        let (c1, r1) = ((c0 + 1).min(self.cols - 1), (r0 + 1).min(self.rows - 1));
        let (tx, tz) = (fx - c0 as f32, fz - r0 as f32);
        let g = |r: usize, c: usize| self.base[r * self.cols + c];
        let mix =
            |a: (f32, f32), b: (f32, f32), t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        let top = mix(g(r0, c0), g(r0, c1), tx);
        let bottom = mix(g(r1, c0), g(r1, c1), tx);
        mix(top, bottom, tz)
    }

    /// How far along the route, in metres, the road nearest `(x, z)` is —
    /// from the nearest point of the coarse grid, never blended: between two
    /// stretches of a lap, a blend would name a third that is nowhere near.
    pub fn road_along(&self, x: f32, z: f32) -> f32 {
        self.along
            .get(self.nearest_cell(x, z))
            .copied()
            .unwrap_or(0.0)
    }

    /// Whether `(x, z)` is left of the road nearest it, going the way of
    /// the ride — on the coarse grid, like [`World::road_along`].
    pub fn left_of_road(&self, x: f32, z: f32) -> bool {
        self.left
            .get(self.nearest_cell(x, z))
            .copied()
            .unwrap_or(false)
    }

    /// The coarse grid point nearest `(x, z)`, as an index.
    fn nearest_cell(&self, x: f32, z: f32) -> usize {
        let at = |v: f32, lo: f32, n: usize| {
            let f = ((v - lo) / BASE_CELL_M).round();
            if f.is_finite() {
                f.clamp(0.0, (n - 1) as f32) as usize
            } else {
                0
            }
        };
        at(z, self.z0, self.rows) * self.cols + at(x, self.x0, self.cols)
    }

    /// Roughly how far `(x, z)` is from the road, in metres.
    pub fn road_distance(&self, x: f32, z: f32) -> f32 {
        self.base_at(x, z).1
    }

    /// The land's own height at `(x, z)`, before it is fitted to the road:
    /// the road's height carried out over the land, plus hills that grow
    /// with distance from the road.
    ///
    /// Each biome shapes its land its own way; where two meet, their
    /// heights blend by weight. Water lies dead flat whatever the blend.
    pub fn height(&self, x: f32, z: f32) -> f32 {
        let (base, from_road) = self.base_at(x, z);
        if let Some((level, _)) = self.water_at(x, z, base, from_road, from_road) {
            return level;
        }
        let Some(map) = &self.biomes else {
            return self.relief(Relief::Rolling, 1.0, base, from_road, x, z);
        };
        let w = map.weights_at(x, z);
        let (mut sum, mut total) = (0.0, 0.0);
        for b in Biome::ALL {
            let k = w[b as usize];
            if k.is_nan() || k <= 0.01 {
                continue;
            }
            let rules = &b.def().land;
            let mut h = self.relief(rules.relief, rules.relief_scale, base, from_road, x, z);
            if rules.water == WaterKind::Sea {
                h = self.shore(h, base, x, z, from_road);
            }
            sum += k * h;
            total += k;
        }
        if total > 0.0 {
            sum / total
        } else {
            self.relief(Relief::Rolling, 1.0, base, from_road, x, z)
        }
    }

    /// The land's height in one kind of relief, `scale` times as high.
    fn relief(&self, relief: Relief, scale: f32, base: f32, from_road: f32, x: f32, z: f32) -> f32 {
        let t = smoothstep(HILLS_FROM_M, HILLS_FULL_M, from_road);
        let rolling = fbm(self.seed ^ 0x4111, x, z, 380.0, 4);
        let broad = fbm(self.seed ^ 0xB120, x, z, 1_600.0, 3).max(0.0);
        let hills = NEAR_HILLS_M * rolling + (FAR_HILLS_M * broad + 14.0 * rolling) * t;
        match relief {
            Relief::Rolling => base + scale * hills,
            Relief::Flat => base + scale * NEAR_HILLS_M * rolling,
            Relief::Dunes => {
                // Crests across a wind that blows one way over the whole
                // world: noise stretched along the crest.
                let a = unit(splitmix(self.seed ^ 0x3171D)) * std::f32::consts::PI;
                let (c, s) = (a.cos(), a.sin());
                let (u, v) = (x * c + z * s, z * c - x * s);
                let n = fbm(self.seed ^ 0xD0E, u * 0.3, v, 150.0, 3);
                let crest = (1.0 - n.abs() * 2.5).max(0.0);
                let dunes = (4.0 + 18.0 * smoothstep(40.0, 500.0, from_road)) * crest * crest;
                // Far off, now and then, a flat-topped mesa.
                let m = fbm(self.seed ^ 0x3E5A, x, z, 700.0, 2);
                let mesa = 55.0 * smoothstep(0.2, 0.26, m) * smoothstep(400.0, 1_200.0, from_road);
                base + scale * (dunes + 0.4 * hills + mesa)
            }
            Relief::Terraces => {
                // Hillsides cut into steps three metres high, flat treads
                // with a short bank at the back.
                let rel = scale * hills;
                let step = 3.0;
                let q = (rel / step).floor();
                let stepped = (q + smoothstep(0.7, 1.0, rel / step - q)) * step;
                base + lerp(rel, stepped, smoothstep(50.0, 150.0, from_road))
            }
            Relief::Peaks => {
                // Ridges and peaks that start close to the road.
                let near = smoothstep(80.0, 1_000.0, from_road);
                let r = 1.0 - (fbm(self.seed ^ 0x9EA, x, z, 900.0, 4) * 2.0).abs();
                let ridged = r.clamp(0.0, 1.0);
                base + scale
                    * (2.0 * NEAR_HILLS_M * rolling
                        + (260.0 * ridged.powi(3) + 40.0 * broad + 20.0 * rolling) * near)
            }
        }
    }

    /// The land `h` brought down to a beach on the sea side of the road.
    fn shore(&self, h: f32, base: f32, x: f32, z: f32, from_road: f32) -> f32 {
        if !self.sea_side(x, z) {
            return h;
        }
        lerp(
            h,
            base - BEACH_DROP_M,
            smoothstep(SEA_FROM_M - 50.0, SEA_FROM_M - 10.0, from_road),
        )
    }

    /// Whether the sea lies this side of the road at `(x, z)`: one side,
    /// seeded, for the whole ride, as a coast road runs.
    fn sea_side(&self, x: f32, z: f32) -> bool {
        self.left_of_road(x, z) == (splitmix(self.seed ^ 0x5EA) & 1 == 0)
    }

    /// Open water at `(x, z)`, if its biome has any there: its flat level
    /// and its kind. Decided on the world's own coarse measure of the
    /// distance to the road, which [`World::height`] uses too, so where
    /// the land is water it is level — and on `exact`, the caller's own
    /// measure where it has one: the coarse one can be tens of metres out,
    /// and water near the road would stand above it (rule 7). Where the
    /// two disagree the land is laid level but is not water.
    fn water_at(
        &self,
        x: f32,
        z: f32,
        base: f32,
        coarse: f32,
        exact: f32,
    ) -> Option<(f32, AreaKind)> {
        let from_road = coarse.min(exact);
        let rules = self.biomes.as_ref()?;
        let rules = &rules.dominant_at(x, z).def().land;
        match rules.water {
            WaterKind::Sea if from_road > SEA_FROM_M && self.sea_side(x, z) => {
                Some((base - SEA_DROP_M, AreaKind::Water))
            }
            WaterKind::Pools
                if from_road > POOL_FROM_M && fbm(self.seed ^ 0x9001, x, z, 90.0, 3) > -0.02 =>
            {
                Some((base - POOL_DROP_M, AreaKind::Water))
            }
            WaterKind::Oasis
                if from_road > OASIS_FROM_M && fbm(self.seed ^ 0x0A51, x, z, 160.0, 2) > 0.46 =>
            {
                Some((base - POOL_DROP_M, AreaKind::Water))
            }
            _ => None,
        }
    }

    /// Sand at `(x, z)`: the beach before the sea, the ring round an oasis.
    fn sand_at(&self, x: f32, z: f32, rules: &LandRules, from_road: f32) -> bool {
        match rules.water {
            WaterKind::Sea => from_road > SEA_FROM_M - 40.0 && self.sea_side(x, z),
            WaterKind::Oasis => {
                from_road > OASIS_FROM_M - 20.0 && fbm(self.seed ^ 0x0A51, x, z, 160.0, 2) > 0.38
            }
            _ => false,
        }
    }

    /// What covers the land at `(x, z)`: woods on patches that grow larger
    /// away from the road, fields elsewhere in patches, grass between.
    pub fn land(&self, x: f32, z: f32) -> Option<AreaKind> {
        self.land_at(x, z, self.road_distance(x, z))
    }

    /// [`World::land`] for a place known to be `from_road` metres from the
    /// road's line — exactly, where the caller can measure it: the world's
    /// own measure cuts the corners of bends.
    pub fn land_at(&self, x: f32, z: f32, from_road: f32) -> Option<AreaKind> {
        let rules = self.rules_at(x, z);
        let (base, coarse) = self.base_at(x, z);
        if let Some((_, kind)) = self.water_at(x, z, base, coarse, from_road) {
            return Some(kind);
        }
        if self.sand_at(x, z, rules, coarse.min(from_road)) {
            return Some(AreaKind::Sand);
        }
        if from_road < rules.paving_m {
            return Some(AreaKind::Paving);
        }
        let wood = fbm(self.seed ^ 0x3009, x, z, 320.0, 3);
        // Woods keep back from the road, and gather on the far hills.
        let wood_line = 0.42 - 0.22 * smoothstep(200.0, 2_000.0, from_road) - rules.wood;
        if from_road > rules.wood_from_m && wood > wood_line {
            return Some(AreaKind::Wood);
        }
        // Fields keep back behind a grass verge.
        if from_road > VERGE_M && fbm(self.seed ^ 0xF1E1, x, z, 160.0, 3) > rules.fields {
            return Some(AreaKind::Farmland);
        }
        None
    }

    /// Straight ditches between the fields wherever the biome digs them:
    /// alongside the road at a few distances, and across the land away from
    /// it every so often — never within `clear_m` of the road, by
    /// `from_road(x, z)`.
    pub fn ditches(
        &self,
        road: &[RoadPoint],
        clear_m: f32,
        from_road: impl Fn(f32, f32) -> f32,
    ) -> Vec<Vec<(f32, f32)>> {
        let mut out = Vec::new();
        let (Some(first), Some(last)) = (road.first(), road.last()) else {
            return out;
        };
        let dug = |x: f32, z: f32| {
            self.rules_at(x, z).water == WaterKind::Ditches && from_road(x, z) >= clear_m
        };
        // Cut a line into the stretches that may be dug.
        let mut keep = |points: &mut dyn Iterator<Item = (f32, f32)>| {
            let mut line = Vec::new();
            for (x, z) in points {
                if dug(x, z) {
                    line.push((x, z));
                } else if line.len() >= 2 {
                    out.push(std::mem::take(&mut line));
                } else {
                    line.clear();
                }
            }
            if line.len() >= 2 {
                out.push(line);
            }
        };
        let length = last.distance_m - first.distance_m;
        if !(length.is_finite() && clear_m.is_finite()) {
            return Vec::new();
        }
        let steps = (length / DITCH_STEP_M) as usize;
        for off in [clear_m + 6.0, clear_m + 80.0, clear_m + 190.0] {
            for side in [-1.0, 1.0] {
                keep(&mut (0..=steps).filter_map(|k| {
                    beside(road, first.distance_m + k as f32 * DITCH_STEP_M, side * off)
                }));
            }
        }
        let mut along = first.distance_m + DITCH_ACROSS_EVERY_M / 2.0;
        while along < last.distance_m {
            for side in [-1.0, 1.0] {
                let (Some(a), Some(b)) = (beside(road, along, 0.0), beside(road, along, side))
                else {
                    continue;
                };
                let (dx, dz) = (b.0 - a.0, b.1 - a.1);
                keep(&mut (0..=(600.0 / DITCH_STEP_M) as usize).map(|k| {
                    let o = clear_m + k as f32 * DITCH_STEP_M;
                    (a.0 + dx * o, a.1 + dz * o)
                }));
            }
            along += DITCH_ACROSS_EVERY_M;
        }
        out
    }

    /// Meandering streams, as lines, that never come within `clear_m` of
    /// the road: `from_road(x, z)` says how far a place is from it. About
    /// one stream for every `every_m` of route, starting beside it and
    /// wandering roughly alongside, as brooks run along a valley road.
    pub fn streams(
        &self,
        road: &[RoadPoint],
        every_m: f32,
        clear_m: f32,
        from_road: impl Fn(f32, f32) -> f32,
    ) -> Vec<Vec<(f32, f32)>> {
        let mut out = Vec::new();
        let (Some(first), Some(last)) = (road.first(), road.last()) else {
            return out;
        };
        if every_m.is_nan() || every_m <= 0.0 {
            return out;
        }
        let mut d = first.distance_m + every_m * 0.5;
        let mut k = 0u64;
        while d < last.distance_m {
            let h = splitmix(self.seed ^ 0x5733 ^ k);
            k += 1;
            let at = d;
            d += every_m * (0.6 + 0.8 * unit(h.rotate_left(5)));
            let i = road
                .partition_point(|p| p.distance_m <= at)
                .clamp(1, road.len() - 1);
            let (a, b) = (road[i - 1].pos, road[i].pos);
            let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
            let len = dx.hypot(dz);
            if len.is_nan() || len <= 1e-3 {
                continue;
            }
            let (hx, hz) = (dx / len, dz / len);
            let side = if h & 1 == 0 { 1.0 } else { -1.0 };
            let off = 45.0 + 260.0 * unit(h.rotate_left(17));
            let (mut x, mut z) = (a[0] - hz * off * side, a[2] + hx * off * side);
            // Heading: along the road, give or take.
            let mut heading = hz.atan2(hx) + (unit(h.rotate_left(29)) - 0.5) * 0.8;
            let steps = 30 + (unit(h.rotate_left(41)) * 40.0) as usize;
            let mut line: Vec<(f32, f32)> = Vec::new();
            for s in 0..steps {
                if from_road(x, z) >= clear_m {
                    line.push((x, z));
                } else if line.len() >= 2 {
                    out.push(std::mem::take(&mut line));
                } else {
                    line.clear();
                }
                heading += fbm(h, s as f32, 0.0, 6.0, 2) * 0.45;
                x += heading.cos() * 12.0;
                z += heading.sin() * 12.0;
            }
            if line.len() >= 2 {
                out.push(line);
            }
        }
        out
    }
}

/// The point `along` metres down the road, `off` metres to its right
/// (negative: left), or `None` off the road's ends.
pub fn beside(road: &[RoadPoint], along: f32, off: f32) -> Option<(f32, f32)> {
    let i = road.partition_point(|p| p.distance_m <= along);
    if i == 0 || i >= road.len() {
        return None;
    }
    let (a, b) = (road[i - 1].pos, road[i].pos);
    let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
    let len = dx.hypot(dz);
    if len.is_nan() || len <= 1e-3 {
        return None;
    }
    let span = road[i].distance_m - road[i - 1].distance_m;
    let t = if span > 1e-3 {
        ((along - road[i - 1].distance_m) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (x, z) = (a[0] + dx * t, a[2] + dz * t);
    // Right of travel: (−dz, dx).
    Some((x - dz / len * off, z + dx / len * off))
}

impl World {
    /// Rows of poplars along the road, now and then, on one side or the
    /// other: about one row for every `every_m` of route, `60–180` m long,
    /// between `near_m` and `near_m + 30` m from the road's line, following
    /// its bends.
    pub fn poplar_rows(
        &self,
        road: &[RoadPoint],
        every_m: f32,
        near_m: f32,
    ) -> Vec<Vec<(f32, f32)>> {
        let mut out = Vec::new();
        let (Some(first), Some(last)) = (road.first(), road.last()) else {
            return out;
        };
        if every_m.is_nan() || every_m <= 0.0 {
            return out;
        }
        let mut d = first.distance_m + every_m * 0.3;
        let mut k = 0u64;
        while d < last.distance_m {
            let h = splitmix(self.seed ^ 0x9091 ^ k);
            k += 1;
            let start = d;
            d += every_m * (0.5 + unit(h.rotate_left(3)));
            if unit(h) < 0.35 {
                continue;
            }
            let side = if h & 4 == 0 { 1.0 } else { -1.0 };
            let off = side * (near_m + 30.0 * unit(h.rotate_left(11)));
            let len = 60.0 + 120.0 * unit(h.rotate_left(23));
            let row: Vec<(f32, f32)> = (0..=(len / 9.0) as usize)
                .filter_map(|s| beside(road, start + s as f32 * 9.0, off))
                .collect();
            if row.len() >= 2 {
                out.push(row);
            }
        }
        out
    }
}

/// Squared distance from `(x, z)` to the segment `a`–`b`, and how far
/// along the segment, 0–1, its nearest point is.
fn segment_d2_t(a: (f32, f32), b: (f32, f32), x: f32, z: f32) -> (f32, f32) {
    let (dx, dz) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dz * dz;
    let t = if len2 > 0.0 {
        (((x - a.0) * dx + (z - a.1) * dz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((a.0 + t * dx - x).powi(2) + (a.1 + t * dz - z).powi(2), t)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight road due north at `height`, one point every 10 m.
    fn north_road(points: usize, height: impl Fn(usize) -> f32) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [0.0, height(i), -(i as f32) * 10.0],
                distance_m: i as f32 * 10.0,
            })
            .collect()
    }

    #[test]
    fn should_keep_noise_within_bounds_anywhere() {
        for k in 0..2_000 {
            let (x, z) = (k as f32 * 37.3 - 30_000.0, k as f32 * -19.7 + 5_000.0);
            let n = fbm(9, x, z, 300.0, 4);
            assert!((-1.0..=1.0).contains(&n), "{n} at {x}, {z}");
        }
        assert_eq!(fbm(9, f32::NAN, 0.0, 300.0, 4), 0.0);
        assert_eq!(fbm(9, 1.0, 1.0, 300.0, 0), 0.0);
    }

    #[test]
    fn should_vary_smoothly_not_in_steps() {
        let mut last = fbm(3, 0.0, 0.0, 300.0, 4);
        for k in 1..2_000 {
            let now = fbm(3, k as f32 * 0.5, 0.0, 300.0, 4);
            assert!((now - last).abs() < 0.05, "jumped at {}", k as f32 * 0.5);
            last = now;
        }
    }

    #[test]
    fn should_carry_the_roads_height_out_over_the_land() {
        // A road climbing from 0 to 300 m: the land beside its foot is low,
        // beside its top high — never the route's average everywhere.
        let road = north_road(301, |i| i as f32);
        let w = World::new(&road);
        let foot = w.height(150.0, 0.0);
        let top = w.height(150.0, -3_000.0);
        assert!(foot < 40.0, "foot {foot}");
        assert!(top > 260.0, "top {top}");
    }

    #[test]
    fn should_know_how_far_along_the_ride_the_nearest_road_is() {
        let road = north_road(301, |_| 0.0);
        let w = World::new(&road);
        for z in [0.0, -1_234.0, -2_990.0] {
            let along = w.road_along(400.0, z);
            assert!((along + z).abs() <= 50.0, "{along} beside {z}");
        }
        // Beyond either end, the end.
        assert!(w.road_along(0.0, 2_000.0) <= 50.0);
        assert!(w.road_along(0.0, -6_000.0) >= 2_950.0);
        assert!(w.road_along(f32::NAN, f32::INFINITY).is_finite());
    }

    #[test]
    fn should_name_the_nearer_leg_of_an_out_and_back_not_a_blend() {
        // North 3 km at x = 0, east 600 m, back south 3 km at x = 600.
        let mut road = north_road(301, |_| 0.0);
        for i in 1..=60 {
            road.push(RoadPoint {
                pos: [i as f32 * 10.0, 0.0, -3_000.0],
                distance_m: 3_000.0 + i as f32 * 10.0,
            });
        }
        for i in 1..=300 {
            road.push(RoadPoint {
                pos: [600.0, 0.0, -3_000.0 + i as f32 * 10.0],
                distance_m: 3_600.0 + i as f32 * 10.0,
            });
        }
        let w = World::new(&road);
        let back = w.road_along(560.0, -1_000.0);
        assert!((back - 5_600.0).abs() <= 50.0, "return leg: {back}");
        let out = w.road_along(40.0, -1_000.0);
        assert!((out - 1_000.0).abs() <= 50.0, "outward leg: {out}");
    }

    /// A flat road due north, its world shaped and covered as `biome`.
    fn world_of(biome: Biome) -> (World, Vec<RoadPoint>) {
        let road = north_road(401, |_| 10.0);
        let mut w = World::new(&road);
        w.set_biomes(BiomeMap::uniform(biome));
        (w, road)
    }

    /// Places on a grid either side of the road, from `near` to `far`
    /// metres out.
    fn beside_road(near: f32, far: f32) -> impl Iterator<Item = (f32, f32)> {
        (0..40).flat_map(move |i| {
            (0..30).flat_map(move |j| {
                let x = near + (far - near) * j as f32 / 29.0;
                let z = -(i as f32) * 100.0;
                [(x, z), (-x, z)]
            })
        })
    }

    #[test]
    fn should_lay_a_polder_dead_level_to_the_horizon() {
        let (w, _) = world_of(Biome::Polder);
        for (x, z) in beside_road(30.0, 3_000.0) {
            let h = w.height(x, z);
            assert!((h - 10.0).abs() < 1e-3, "{h} at {x}, {z}");
        }
    }

    #[test]
    fn should_raise_mountains_well_above_the_road_and_dunes_above_the_desert() {
        let (mountain, _) = world_of(Biome::Mountain);
        let peak = beside_road(200.0, 3_000.0)
            .map(|(x, z)| mountain.height(x, z))
            .fold(f32::MIN, f32::max);
        assert!(peak > 150.0, "highest mountain only {peak} m");
        let (desert, _) = world_of(Biome::Desert);
        let (countryside, _) = world_of(Biome::Temperate);
        let differs = beside_road(150.0, 600.0)
            .filter(|&(x, z)| (desert.height(x, z) - countryside.height(x, z)).abs() > 2.0)
            .count();
        assert!(
            differs > 200,
            "the desert is shaped like the countryside ({differs})"
        );
    }

    #[test]
    fn should_grow_no_woods_or_fields_in_the_desert_and_pave_the_town() {
        let (desert, _) = world_of(Biome::Desert);
        for (x, z) in beside_road(10.0, 1_500.0) {
            let kind = desert.land_at(x, z, x.abs());
            assert!(
                !matches!(kind, Some(AreaKind::Wood | AreaKind::Farmland)),
                "{kind:?} at {x}, {z}"
            );
        }
        let (town, _) = world_of(Biome::Urban);
        assert_eq!(town.land_at(30.0, -500.0, 30.0), Some(AreaKind::Paving));
        assert_eq!(town.land_at(-89.0, -500.0, 89.0), Some(AreaKind::Paving));
        assert_ne!(town.land_at(-91.0, -500.0, 91.0), Some(AreaKind::Paving));
    }

    #[test]
    fn should_put_the_sea_on_one_side_of_the_road_only_and_never_near_it() {
        let (coast, _) = world_of(Biome::Coast);
        let wet = |x: f32| {
            (0..30)
                .filter(|&i| {
                    let z = -100.0 - i as f32 * 100.0;
                    coast.land_at(x, z, x.abs()) == Some(AreaKind::Water)
                })
                .count()
        };
        let (east, west) = (wet(300.0), wet(-300.0));
        assert!(
            (east == 30 && west == 0) || (east == 0 && west == 30),
            "sea east {east}, west {west}"
        );
        // Not within 64 m of the road's line, by any measure: the exact one
        // wins even where the coarse grid says otherwise.
        for x in [-64.0, -45.0, -30.0, 30.0, 45.0, 64.0] {
            assert_eq!(wet(x), 0, "sea at {x}");
            for z in [-500.0, -1_500.0] {
                assert_ne!(coast.land_at(x, z, 10.0), Some(AreaKind::Water));
            }
        }
    }

    #[test]
    fn should_keep_the_land_near_the_road_close_to_its_height() {
        let road = north_road(300, |_| 50.0);
        let w = World::new(&road);
        for k in 0..100 {
            let z = -(k as f32) * 29.0;
            let h = w.height(20.0, z);
            assert!((h - 50.0).abs() <= NEAR_HILLS_M + 0.5, "{h} at z {z}");
        }
    }

    #[test]
    fn should_raise_bigger_hills_far_from_the_road() {
        let road = north_road(300, |_| 0.0);
        let w = World::new(&road);
        let spread = |x: f32| {
            let hs: Vec<f32> = (0..200).map(|k| w.height(x, -(k as f32) * 15.0)).collect();
            hs.iter().cloned().fold(f32::MIN, f32::max)
                - hs.iter().cloned().fold(f32::MAX, f32::min)
        };
        assert!(
            spread(2_800.0) > 3.0 * spread(30.0),
            "{} vs {}",
            spread(2_800.0),
            spread(30.0)
        );
    }

    #[test]
    fn should_know_how_far_the_road_is() {
        let road = north_road(300, |_| 0.0);
        let w = World::new(&road);
        // Between two anchors, on the road itself, it is near — within the
        // grid's own blur, not the 125 m an anchor can be away.
        for k in 0..30 {
            let z = -(k as f32) * 97.0;
            assert!(
                w.road_distance(0.0, z) < 30.0,
                "{} at z {z}",
                w.road_distance(0.0, z)
            );
        }
        assert!((w.road_distance(1_000.0, -1_500.0) - 1_000.0).abs() < 150.0);
    }

    #[test]
    fn should_keep_a_grass_verge_beside_the_road() {
        let road = north_road(300, |_| 0.0);
        let w = World::new(&road);
        for k in 0..3_000 {
            let (x, z) = ((k % 44) as f32 - 22.0, -(k as f32));
            assert_eq!(w.land_at(x, z, x.abs()), None, "at {x}, {z}");
        }
    }

    #[test]
    fn should_keep_woods_off_the_roadside() {
        let road = north_road(300, |_| 0.0);
        let w = World::new(&road);
        for k in 0..3_000 {
            let (x, z) = ((k % 60) as f32 - 30.0, -(k as f32));
            if w.road_distance(x, z) <= 50.0 {
                assert_ne!(w.land(x, z), Some(AreaKind::Wood), "at {x}, {z}");
            }
        }
    }

    #[test]
    fn should_grow_fields_woods_and_grass_all_three() {
        let road = north_road(500, |_| 0.0);
        let w = World::new(&road);
        let mut seen = std::collections::HashSet::new();
        for k in 0..4_000 {
            let (x, z) = ((k % 80) as f32 * 30.0 - 1_200.0, -((k / 80) as f32) * 100.0);
            seen.insert(w.land(x, z));
        }
        for kind in [None, Some(AreaKind::Wood), Some(AreaKind::Farmland)] {
            assert!(seen.contains(&kind), "no {kind:?}");
        }
    }

    #[test]
    fn should_grow_the_same_world_from_the_same_route_and_another_from_another() {
        let a = World::new(&north_road(200, |i| i as f32 * 0.1));
        let b = World::new(&north_road(200, |i| i as f32 * 0.1));
        assert_eq!(a, b);
        let c = World::new(&north_road(201, |i| i as f32 * 0.1));
        assert_ne!(a.seed(), c.seed());
    }

    #[test]
    fn should_keep_every_stream_clear_of_the_road() {
        let road = north_road(800, |_| 0.0);
        let w = World::new(&road);
        let from_road = |x: f32, _: f32| x.abs();
        let streams = w.streams(&road, 500.0, 35.0, from_road);
        assert!(!streams.is_empty());
        for s in &streams {
            assert!(s.len() >= 2);
            for &(x, z) in s {
                assert!(from_road(x, z) >= 35.0, "stream at {x}, {z}");
            }
        }
    }

    #[test]
    fn should_line_poplar_rows_beside_the_road_not_on_it() {
        let road = north_road(800, |_| 0.0);
        let w = World::new(&road);
        let rows = w.poplar_rows(&road, 300.0, 14.0);
        assert!(rows.len() >= 5, "{} rows", rows.len());
        for row in &rows {
            for &(x, _) in row {
                assert!(
                    (14.0..=44.0).contains(&x.abs()),
                    "a poplar {x} m off the line"
                );
            }
        }
        assert_eq!(rows, w.poplar_rows(&road, 300.0, 14.0));
    }

    #[test]
    fn should_find_the_point_beside_the_road_on_its_right() {
        // Heading north (−z), the right is east (+x).
        let road = north_road(10, |_| 0.0);
        let (x, z) = beside(&road, 25.0, 4.0).expect("on the road");
        assert!(
            (x - 4.0).abs() < 1e-4 && (z + 25.0).abs() < 1e-4,
            "{x}, {z}"
        );
        assert_eq!(beside(&road, -1.0, 4.0), None);
        assert_eq!(beside(&road, 1_000.0, 4.0), None);
    }

    #[test]
    fn should_survive_a_route_of_nothing() {
        let w = World::new(&[]);
        assert!(w.height(0.0, 0.0).is_finite());
        let nan = World::new(&north_road(3, |_| f32::NAN));
        assert!(nan.height(5.0, 5.0).is_finite());
        assert!(w.streams(&[], 500.0, 35.0, |_, _| 100.0).is_empty());
        let road = north_road(10, |_| 0.0);
        assert!(w.streams(&road, 0.0, 35.0, |_, _| 100.0).is_empty());
    }
}
