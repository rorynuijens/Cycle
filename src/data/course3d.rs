//! Geometry for the 3D rider's-eye view of a route.
//!
//! Pure: no GTK and no GL. This module turns a [`Route`] into triangles and
//! answers "where is the rider's eye at this distance"; the widget in
//! `ui/widgets/course_view.rs` only uploads and draws what it is given.
//!
//! Scene space is metres around the middle of the route, with x east, y up and
//! z south — so north is −z, the direction an OpenGL camera looks by default.
//! f32 is enough: 50 km from the origin it still resolves 4 mm.
//!
//! The road follows the GPX's own elevation, because the GPX is what the
//! trainer is driven from: the road the rider sees climbing is the climb they
//! feel. Everything around it is a made-up world (`data/world.rs`) grown from
//! the route, with no map data and no network; the ground is fitted to the
//! road, never the other way round.

use crate::data::biome::{self, BiomeDef, BiomeMap, Scenery};
use crate::data::palette::Palette;
use crate::data::props::Prop;
use crate::data::roadway;
use crate::data::route::Route;
use crate::data::world::{splitmix, AreaKind, World};
use crate::data::{animals, dressing, encounters, rider, sky, track, trees};

/// The chase camera: this far behind the rider and this high above the road,
/// aimed at a point this far ahead of the rider and this high.
pub const CHASE_BACK_M: f32 = 4.0;
pub const CHASE_UP_M: f32 = 2.1;
pub const CHASE_AHEAD_M: f32 = 14.0;
pub const CHASE_AIM_M: f32 = 0.9;

/// How far right of the road's line the rider rides: the middle of the
/// track's right-hand lane.
pub const RIDER_OFFSET_M: f32 = roadway::TRACK_HALF_WIDTH_M / 2.0;

/// The ground grid: vertex spacing, how far either side of the road it reaches,
/// and how far below the road it sits so the road surface always wins the depth
/// test even where the grid's straight edges cut under a bend.
pub const GROUND_SPACING_M: f32 = 20.0;
pub const GROUND_RADIUS_M: f32 = 1_200.0;
pub const GROUND_DROP_M: f32 = 0.3;

/// Ground this close to the road is held flat just under it, whatever the
/// terrain does: this much beyond however far the road reaches from the
/// rider's line at that point. The margin is what makes that a guarantee
/// rather than a hope: any grid triangle over the road surface has its corners
/// within one cell diagonal of it, the surface is within the road's reach of
/// the line, and the line within half a resampling step of a measured point —
/// so all three corners are held, and the triangle cannot rise through the road.
pub const SHOULDER_BASE_M: f32 = 31.0;

/// How far the road reaches from the rider's line: the track's half width.
pub const DEFAULT_EXTENT_M: f32 = roadway::TRACK_HALF_WIDTH_M;

/// The shoulder beside that road.
#[cfg(test)]
pub const SHOULDER_M: f32 = SHOULDER_BASE_M + DEFAULT_EXTENT_M;

/// Spacing the road's line is resampled at before distances to it are
/// taken, in metres.
const RESAMPLE_M: f32 = 5.0;

const _: () =
    assert!(SHOULDER_BASE_M >= GROUND_SPACING_M * std::f32::consts::SQRT_2 + RESAMPLE_M / 2.0);

/// Road within reach of a place that turns by more than this, in radians
/// (20°), holds the ground under all of it — see `world_mesh`. A bend gentler
/// than about 200 m does not, so the ground beside a long climb stays at the
/// road's height instead of being dug into a trench.
const BENT_RAD: f32 = 20.0 * std::f32::consts::PI / 180.0;

/// Beyond the shoulder the terrain blends in, reaching its own height here.
const BLEND_END_M: f32 = 120.0;

/// The elevation tiles and the GPX rarely agree on height to the metre — a
/// barometer drifts, a tile averages a valley. Near the road the land is
/// shifted by the local difference so the road sits on it; by this distance
/// the shift has eased to the route's typical difference, so two stretches of
/// road that disagree do not tear the land between them.
const LOCAL_FIT_END_M: f32 = 600.0;

/// Streams keep at least this far from the road's edge, so no water is ever
/// near enough the road to stand above it (the stylized-route skill, rule 7).
pub const STREAM_CLEAR_M: f32 = 30.0;
/// A stream's half width.
const STREAM_HALF_M: f32 = 1.6;
/// Streams lie this far above the ground under them, so they show on a
/// slope.
const STREAM_LIFT_M: f32 = 0.12;
/// Of the route, about one stream per this many metres, and one row of
/// poplars per this many.
const STREAM_EVERY_M: f32 = 420.0;
const ROW_EVERY_M: f32 = 300.0;
/// Rows of poplars stand at least this far from the road's line.
const ROW_NEAR_M: f32 = 14.0;

/// Past the fine ground, coarser ground reaches out to the world's edge.
const FAR_SPACING_M: f32 = 80.0;
/// Where the coarse ground overlaps the fine, it sits this far lower, so the
/// fine ground is the one seen.
const FAR_DROP_M: f32 = 4.0;

/// What a vertex is made of. Sent to the shader in the colour's alpha, so one
/// vertex layout carries everything.
pub const MATERIAL_PLAIN: f32 = 0.0;
pub const MATERIAL_GROUND: f32 = 1.0;
/// Road surface.
pub const MATERIAL_ROAD: f32 = 2.0;
/// Open water: lighter stripes drift across it.
pub const MATERIAL_WATER: f32 = 4.0;
/// The far backdrop: mountains on the horizon, half lost in the haze.
pub const MATERIAL_BACKDROP: f32 = 5.0;
/// Clouds: lit white, no haze.
pub const MATERIAL_CLOUD: f32 = 6.0;

/// Ground steeper than this (rise over run, about 50°) is bare rock;
/// gentler banks stay green.
const ROCK_SLOPE: f32 = 1.2;

/// Fields where the map has farmland: patches about this far across.
const FIELD_M: f32 = 80.0;

/// How far the ground may stand above or below the road `t` metres beyond
/// its edge (the stylized-route skill, §4.2): nothing within 10 m, then
/// widening, slowly and then fast.
fn bank_band(t: f32) -> f32 {
    0.12 * (t - 10.0).max(0.0) + 0.06 * (t - 25.0).max(0.0).powi(2)
}

/// How the land beside the road looks: which biome is where, and how high
/// snow lies.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub biomes: BiomeMap,
    pub snow_line_m: f32,
    /// Woods thin out with distance from this far, whatever their biome
    /// says: the countryside's are planted as they always were.
    pub thin_from_m: Option<f32>,
}

impl Look {
    /// Temperate everywhere and no snow: the look with nothing to go on.
    #[cfg(test)]
    pub fn plain() -> Self {
        Self {
            biomes: BiomeMap::uniform(biome::Biome::Temperate),
            snow_line_m: f32::INFINITY,
            thin_from_m: None,
        }
    }
}

/// The colour of open ground of `area` in a biome, `rise` metres above the
/// road beside it; water is water, whatever the biome.
fn ground_colour(def: &BiomeDef, area: Option<AreaKind>, x: f32, z: f32, rise: f32) -> [f32; 4] {
    let p = &def.palette;
    let rgb = match area {
        Some(AreaKind::Water) => return [p.water[0], p.water[1], p.water[2], MATERIAL_WATER],
        Some(AreaKind::Farmland) => field(p, def.land.crops, x, z),
        Some(AreaKind::Wood) => p.grass_high,
        Some(AreaKind::Sand) => p.sand,
        Some(AreaKind::Paving) => p.paving,
        None => lerp3(p.grass_low, p.grass_high, smoothstep(0.0, 40.0, rise)),
    };
    [rgb[0], rgb[1], rgb[2], MATERIAL_GROUND]
}

/// The crop in the field at `(x, z)`: fields are the cells round seeded
/// points about [`FIELD_M`] apart, each sown at random from the palette's
/// field colours — the same field every ride.
fn field(p: &Palette, crops: f32, x: f32, z: f32) -> [f32; 3] {
    let (ci, cj) = ((x / FIELD_M).floor() as i64, (z / FIELD_M).floor() as i64);
    let mut best = (f32::MAX, 0u64);
    for di in -1..=1 {
        for dj in -1..=1 {
            let (i, j) = (ci + di, cj + dj);
            let h = splitmix(splitmix(i as u64 ^ 0xF1E1D).wrapping_add(j as u64));
            let cx = (i as f32 + 0.15 + 0.7 * biome::unit(h)) * FIELD_M;
            let cz = (j as f32 + 0.15 + 0.7 * biome::unit(splitmix(h))) * FIELD_M;
            let d = (x - cx).powi(2) + (z - cz).powi(2);
            if d < best.0 {
                best = (d, h);
            }
        }
    }
    // Crops scattered through meadows: half the fields are grass in the
    // countryside, and a biome with more or fewer crops turns some over.
    let pick = (best.1 % 8) as usize;
    let crop = [true, true, true, false, false, true, false, false][pick];
    let turn = biome::unit(splitmix(best.1 ^ 0xC0B5));
    let crop = if crop {
        turn >= (0.5 - crops) * 2.0
    } else {
        turn < (crops - 0.5) * 2.0
    };
    let crops_of = [
        p.field_wheat,
        p.field_ploughed,
        p.field_green,
        p.field_wheat,
    ];
    let grass_of = [p.grass_low, p.grass_low, p.grass_high, p.grass_low];
    match (
        crop,
        [true, true, true, false, false, true, false, false][pick],
    ) {
        // Unturned: the countryside's own field, as it always was.
        (true, true) | (false, false) => [
            p.field_wheat,
            p.field_ploughed,
            p.field_green,
            p.grass_low,
            p.grass_low,
            p.field_wheat,
            p.grass_high,
            p.grass_low,
        ][pick],
        (true, false) => crops_of[(best.1 >> 8) as usize % 4],
        (false, true) => grass_of[(best.1 >> 8) as usize % 4],
    }
}

/// The ground grid's layout: vertex spacing, how far it reaches from the
/// road, and how far below the road it is held.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    pub spacing: f32,
    pub radius: f32,
    pub drop: f32,
}

impl Grid {
    pub const VIEW: Grid = Grid {
        spacing: GROUND_SPACING_M,
        radius: GROUND_RADIUS_M,
        drop: GROUND_DROP_M,
    };
}

/// Raw terrain height at a scene position `(x, z)`, where the data reaches.
pub type Terrain<'a> = &'a dyn Fn(f32, f32) -> Option<f32>;

/// Converts between WGS-84 degrees and local metres around one origin.
///
/// Azimuthal equidistant on the sphere the ride's distance is measured on —
/// see [`LocalFrame::to_local`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalFrame {
    lat0: f64,
    lng0: f64,
}

/// The Earth's radius the ride's distance is measured with (`route::haversine`).
/// The scene uses the same sphere, so a stretch of road is as long on screen
/// as on the trainer (stylized-route skill, rule 4).
const EARTH_RADIUS_M: f64 = 6_371_000.0;

impl LocalFrame {
    /// A frame centred on the middle of the route's bounding box, or `None` for
    /// a route with no points.
    pub fn around(route: &Route) -> Option<Self> {
        let first = route.points.first()?;
        let (mut lat_lo, mut lat_hi, mut lng_lo, mut lng_hi) =
            (first.lat, first.lat, first.lng, first.lng);
        for p in &route.points {
            lat_lo = lat_lo.min(p.lat);
            lat_hi = lat_hi.max(p.lat);
            lng_lo = lng_lo.min(p.lng);
            lng_hi = lng_hi.max(p.lng);
        }
        Some(Self::at((lat_lo + lat_hi) / 2.0, (lng_lo + lng_hi) / 2.0))
    }

    /// A frame with its origin at `lat`, `lng`.
    pub fn at(lat: f64, lng: f64) -> Self {
        Self {
            lat0: lat,
            lng0: lng,
        }
    }

    /// `(east, north)` in metres from the origin: the azimuthal equidistant
    /// projection on the ride's sphere. Distance and bearing from the origin
    /// are exact, and a short segment anywhere within 50 km keeps its length
    /// to about one part in 100 000 — so the road drawn is as long as the road
    /// ridden. The angle is taken with haversine terms, which keep their
    /// precision for points a metre from the origin.
    pub fn to_local(self, lat: f64, lng: f64) -> (f64, f64) {
        let (p1, p2) = (self.lat0.to_radians(), lat.to_radians());
        let dl = (lng - self.lng0).to_radians();
        let h = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
        let c = 2.0 * h.sqrt().min(1.0).asin();
        let bearing =
            (dl.sin() * p2.cos()).atan2(p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos());
        let r = EARTH_RADIUS_M * c;
        (r * bearing.sin(), r * bearing.cos())
    }

    /// `(lat, lng)` of a point `east`, `north` metres from the origin — the
    /// exact inverse of [`LocalFrame::to_local`].
    #[cfg(test)]
    pub fn to_geo(self, east: f64, north: f64) -> (f64, f64) {
        let rho = east.hypot(north);
        if rho < 1e-9 {
            return (self.lat0, self.lng0);
        }
        let c = rho / EARTH_RADIUS_M;
        let bearing = east.atan2(north);
        let p1 = self.lat0.to_radians();
        let lat = (p1.sin() * c.cos() + p1.cos() * c.sin() * bearing.cos()).asin();
        let dl = (bearing.sin() * c.sin() * p1.cos()).atan2(c.cos() - p1.sin() * lat.sin());
        (lat.to_degrees(), self.lng0 + dl.to_degrees())
    }
}

/// One point of the smoothed road centreline, in scene space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoadPoint {
    /// x east, y up (the road surface), z south.
    pub pos: [f32; 3],
    /// Distance from the start, in the route's own metres — the same scale
    /// `RouteEngine::distance_m` advances on, so the camera and the ride agree.
    pub distance_m: f32,
}

/// One vertex as the shader reads it: position, then RGB plus a material id.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub colour: [f32; 4],
}

/// Triangles ready to upload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    /// Append `other`, shifting its indices past this mesh's vertices.
    pub fn extend(&mut self, other: Mesh) {
        let base = self.vertices.len() as u32;
        self.vertices.extend(other.vertices);
        self.indices
            .extend(other.indices.into_iter().map(|i| i + base));
    }
}

/// Where the camera is and what it looks at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraPose {
    pub eye: [f32; 3],
    pub target: [f32; 3],
}

/// Where the rider is and which way they face.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RiderPose {
    /// Where the bike touches the road: the middle of the right-hand lane.
    pub pos: [f32; 3],
    /// Heading, in radians: rider space's +x turned to (cos, 0, sin).
    pub yaw: f32,
    /// Nose up on a climb, in radians.
    pub pitch: f32,
    /// Leaning into a bend, in radians: positive to the right.
    pub lean: f32,
    /// How far round the cranks are, in radians.
    pub crank: f32,
    /// How far along the road, in metres: where the camera following takes
    /// its line from.
    pub distance_m: f32,
}

/// Everything the view needs for one route.
#[derive(Debug, Clone, PartialEq)]
pub struct CourseScene {
    pub road: Vec<RoadPoint>,
    pub mesh: Mesh,
    /// Trees and animals, drawn apart from the mesh: many copies of a few
    /// shapes.
    pub props: Vec<Prop>,
    /// The horizon of every biome the ride shows one of, and the clouds,
    /// around the eye — see `data/sky.rs`. Which horizon stands, and how
    /// high, is [`Sky::backdrop`].
    pub horizons: Vec<(biome::Biome, Mesh)>,
    pub clouds: Mesh,
    /// Where along the route each animal encounter happens — see
    /// `data/encounters.rs`.
    pub encounters: Vec<encounters::Trigger>,
    /// The sky every [`SKY_STEP_M`] along the road — see [`CourseScene::sky_at`].
    skies: Vec<Sky>,
}

impl CourseScene {
    /// Build the world for `route`. `None` for a route too short to draw —
    /// fewer than two points.
    pub fn build(route: &Route, scenery: Scenery) -> Option<Self> {
        Self::build_from(route, None, scenery)
    }

    /// Build the world, with the land shaped by `terrain` — any height for
    /// any scene position — instead of the world's own hills.
    pub fn build_from(route: &Route, terrain: Option<Terrain>, scenery: Scenery) -> Option<Self> {
        let road = track::lay(route);
        if road.len() < 2 {
            return None;
        }
        let mut world = World::new(&road);
        let look = look_for(&road, route.points[0].lat, &world, scenery);
        world.set_biomes(look.biomes.clone());
        let own = |x: f32, z: f32| Some(world.height(x, z));
        let terrain = terrain.unwrap_or(&own);
        let mut props = Vec::new();
        // The land and road are drawn on beyond the route's ends; everything
        // that rides — rider, camera, sky, encounters — keeps to `road`.
        let drawn = with_leads(&road);
        let mut mesh = world_mesh(
            &drawn,
            Grid::VIEW,
            Some(terrain),
            Some(&world),
            &look,
            Some(&mut props),
        );
        mesh.extend(roadway::ridden_mesh(&drawn));
        let length = road.last().map_or(0.0, |p| p.distance_m);
        let segments = if scenery == Scenery::Mixed {
            biome::mix_segments(world.seed(), length)
        } else {
            Vec::new()
        };
        let backdrop = |d: f32| biome::backdrop(scenery, &segments, d);
        let skies = skies_along(&road, &look.biomes, &backdrop);
        // A horizon for every biome that stands round the rider anywhere.
        let horizons = biome::Biome::ALL
            .into_iter()
            .filter(|&b| skies.iter().any(|s| s.backdrop[b as usize] > 0.0))
            .map(|b| {
                (
                    b,
                    sky::horizon(world.seed(), &b.def().horizon, &b.def().palette),
                )
            })
            .collect();
        let open = |x: f32, z: f32| matches!(world.land(x, z), None | Some(AreaKind::Farmland));
        let encounters = encounters::triggers(
            &road,
            &|x, z| look.biomes.dominant_at(x, z),
            &open,
            &|x, z| world.land(x, z) == Some(AreaKind::Water),
            world.seed(),
        );
        Some(Self {
            road,
            mesh,
            props,
            horizons,
            clouds: sky::clouds(world.seed()),
            skies,
            encounters,
        })
    }

    /// The sky and haze for a rider at `distance_m`.
    pub fn sky_at(&self, distance_m: f32) -> Sky {
        let Some(first) = self.skies.first() else {
            return Sky::default();
        };
        let k = ((distance_m - self.road[0].distance_m) / SKY_STEP_M).max(0.0);
        if !k.is_finite() {
            return *first;
        }
        let i = (k.floor() as usize).min(self.skies.len() - 1);
        let j = (i + 1).min(self.skies.len() - 1);
        self.skies[i].mix(&self.skies[j], k - k.floor())
    }
}

/// Sky and haze, as the view clears and fogs with them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sky {
    pub top: [f32; 3],
    /// The sky at the horizon, and the haze colour.
    pub horizon: [f32; 3],
    pub fog_density: f32,
    /// How high each biome's horizon stands, 0–1: one at full height, or
    /// one sinking as the next rises on a mixed ride.
    pub backdrop: biome::Weights,
}

impl Default for Sky {
    /// The temperate sky, for a view with no route yet.
    fn default() -> Self {
        Self::of(
            &[0.0; biome::COUNT],
            biome::backdrop(Scenery::Countryside, &[], 0.0),
        )
    }
}

impl Sky {
    fn of(weights: &biome::Weights, backdrop: biome::Weights) -> Self {
        let (top, horizon, fog_density) = biome::sky(weights);
        Self {
            top,
            horizon,
            fog_density,
            backdrop,
        }
    }

    fn mix(&self, o: &Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            top: lerp3(self.top, o.top, t),
            horizon: lerp3(self.horizon, o.horizon, t),
            fog_density: lerp(self.fog_density, o.fog_density, t),
            backdrop: std::array::from_fn(|b| lerp(self.backdrop[b], o.backdrop[b], t)),
        }
    }
}

/// Spacing of the sky samples along the road, in metres.
const SKY_STEP_M: f32 = 50.0;

/// The sky every [`SKY_STEP_M`] along the road, from the biomes at the
/// rider, and the horizon `backdrop(d)` that far along.
fn skies_along(
    road: &[RoadPoint],
    biomes: &BiomeMap,
    backdrop: &dyn Fn(f32) -> biome::Weights,
) -> Vec<Sky> {
    let (Some(first), Some(last)) = (road.first(), road.last()) else {
        return Vec::new();
    };
    let steps = ((last.distance_m - first.distance_m) / SKY_STEP_M)
        .ceil()
        .max(0.0) as usize;
    (0..=steps)
        .filter_map(|k| {
            let d = first.distance_m + k as f32 * SKY_STEP_M;
            position_at(road, d).map(|p| Sky::of(&biomes.weights_at(p[0], p[2]), backdrop(d)))
        })
        .collect()
}

/// The look of the land around `road`: the biome map over everything the
/// world reaches, voted from what grows there and the land's height.
fn look_for(road: &[RoadPoint], lat: f64, world: &World, scenery: Scenery) -> Look {
    let snow_line_m = biome::snow_line_m(lat);
    let segments = match scenery {
        Scenery::Only(b) => {
            return Look {
                biomes: BiomeMap::uniform(b),
                snow_line_m,
                thin_from_m: None,
            }
        }
        Scenery::Mixed => {
            let length = road.last().map_or(0.0, |p| p.distance_m);
            Some(biome::mix_segments(world.seed(), length))
        }
        Scenery::Countryside => None,
    };
    // The countryside is the same wherever the route really is: temperate,
    // woods leaning to forest, high ground to mountain.
    let climate = biome::Biome::Temperate;
    let (mut x_lo, mut x_hi, mut z_lo, mut z_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in road {
        x_lo = x_lo.min(p.pos[0]);
        x_hi = x_hi.max(p.pos[0]);
        z_lo = z_lo.min(p.pos[2]);
        z_hi = z_hi.max(p.pos[2]);
    }
    let r = crate::data::world::REACH_M;
    let biomes = BiomeMap::build((x_lo - r, x_hi + r), (z_lo - r, z_hi + r), |x, z| {
        let weights = match &segments {
            Some(segs) => biome::mix_weights(segs, world.road_along(x, z)),
            None => biome::votes(world.land(x, z), Some(world.height(x, z)), climate),
        };
        (weights, false)
    });
    Look {
        biomes,
        snow_line_m,
        thin_from_m: (scenery == Scenery::Countryside).then_some(COUNTRYSIDE_THIN_FROM_M),
    }
}

/// Where the countryside's woods start to thin out, in metres from the
/// road — as they have since the rider first saw them.
const COUNTRYSIDE_THIN_FROM_M: f32 = 350.0;

/// The road surface point at `distance_m`, clamped to the ends of the road.
/// `None` only for an empty road.
pub fn position_at(road: &[RoadPoint], distance_m: f32) -> Option<[f32; 3]> {
    let first = road.first()?;
    let idx = road.partition_point(|p| p.distance_m <= distance_m);
    if idx == 0 {
        return Some(first.pos);
    }
    let Some(p1) = road.get(idx) else {
        return road.last().map(|p| p.pos);
    };
    let p0 = &road[idx - 1];
    let span = p1.distance_m - p0.distance_m;
    if span < 0.01 {
        return Some(p0.pos);
    }
    let t = (distance_m - p0.distance_m) / span;
    Some(lerp3(p0.pos, p1.pos, t))
}

/// Half the stretch of road the rider's heading is taken over, in metres:
/// the road's own direction under the wheels.
const HEADING_SPAN_M: f32 = 1.5;
/// Half the stretch over which the bend under the rider is measured for
/// their lean, in metres.
const LEAN_SPAN_M: f32 = 4.0;
/// The furthest the rider leans into a bend.
pub const MAX_LEAN_RAD: f32 = 35.0 * std::f32::consts::PI / 180.0;
const GRAVITY_MS2: f32 = 9.81;

/// The chase camera looks along the road's direction averaged from this far
/// behind the rider to this far ahead, in metres: it swings round a bend a
/// little after the rider does, as a following camera would, rather than
/// turning with every metre of the road.
pub const CAMERA_TRAIL_M: f32 = 12.0;
pub const CAMERA_LEAD_M: f32 = 4.0;

/// The road's flat direction at `distance_m`, taken over
/// [`HEADING_SPAN_M`] either side and kept within the road; north for a road
/// with no length.
fn direction_at(road: &[RoadPoint], distance_m: f32) -> Option<[f32; 3]> {
    let start = road.first()?.distance_m;
    let end = road.last()?.distance_m;
    let span = HEADING_SPAN_M.min((end - start) / 2.0);
    let d = distance_m.clamp(start + span, end - span);
    let a = position_at(road, d - span)?;
    let b = position_at(road, d + span)?;
    Some(normalise_xz(sub3(b, a)).unwrap_or([0.0, 0.0, -1.0]))
}

/// Where the rider is at `distance_m`, riding at `speed_ms`: in the middle
/// of the track's right-hand lane, level with that distance on the road's
/// line and at its height, facing along the road under the wheels, and
/// leaning into a bend as far as the speed round it asks — the lean that
/// balances a bike, `tan θ = v² / (g r)`.
pub fn rider_pose(road: &[RoadPoint], distance_m: f32, speed_ms: f32) -> Option<RiderPose> {
    let start = road.first()?.distance_m;
    let end = road.last()?.distance_m;
    let d = distance_m.clamp(start, end);
    let here = position_at(road, d)?;
    let dir = direction_at(road, d)?;
    // The track is centred on the road's line; the rider rides the middle of
    // its right-hand lane, beside the line, at the line's own distance and
    // height — so the ride's numbers are the line's, only the rider moves
    // over.
    let (rx, rz) = (-dir[2] * RIDER_OFFSET_M, dir[0] * RIDER_OFFSET_M);
    // Nose up or down with the road under the wheels.
    let span = 3.0;
    let behind = position_at(road, (d - span).max(start))?;
    let front = position_at(road, (d + span).min(end))?;
    let run = (front[0] - behind[0]).hypot(front[2] - behind[2]);
    let pitch = if run > 0.1 {
        (front[1] - behind[1]).atan2(run)
    } else {
        0.0
    };
    // How sharply the road turns here: right positive, as a turn from east
    // towards south is.
    let (a, b) = (
        direction_at(road, d - LEAN_SPAN_M)?,
        direction_at(road, d + LEAN_SPAN_M)?,
    );
    let turn = (a[0] * b[2] - a[2] * b[0]).atan2(a[0] * b[0] + a[2] * b[2]);
    let curvature = turn / (2.0 * LEAN_SPAN_M);
    let v = if speed_ms.is_finite() {
        speed_ms.max(0.0)
    } else {
        0.0
    };
    let lean = (v * v * curvature / GRAVITY_MS2)
        .atan()
        .clamp(-MAX_LEAN_RAD, MAX_LEAN_RAD);
    Some(RiderPose {
        pos: [here[0] + rx, here[1], here[2] + rz],
        yaw: dir[2].atan2(dir[0]),
        pitch,
        lean,
        crank: rider::crank(d),
        distance_m: d,
    })
}

/// The camera behind and above the rider, looking past them up the road —
/// along the road's direction averaged over the stretch from
/// [`CAMERA_TRAIL_M`] behind the rider to [`CAMERA_LEAD_M`] ahead.
pub fn chase_camera(road: &[RoadPoint], rider: &RiderPose) -> CameraPose {
    let (mut fx, mut fz) = (0.0f32, 0.0f32);
    let mut s = -CAMERA_TRAIL_M;
    while s <= CAMERA_LEAD_M {
        if let Some(dir) = direction_at(road, rider.distance_m + s) {
            fx += dir[0];
            fz += dir[2];
        }
        s += 1.0;
    }
    let (fx, fz) = match normalise_xz([fx, 0.0, fz]) {
        Some(f) => (f[0], f[2]),
        None => (rider.yaw.cos(), rider.yaw.sin()),
    };
    let p = rider.pos;
    CameraPose {
        eye: [
            p[0] - fx * CHASE_BACK_M,
            p[1] + CHASE_UP_M,
            p[2] - fz * CHASE_BACK_M,
        ],
        target: [
            p[0] + fx * CHASE_AHEAD_M,
            p[1] + CHASE_AIM_M,
            p[2] + fz * CHASE_AHEAD_M,
        ],
    }
}

/// A ground grid reaching `radius` either side of the road.
///
/// Without `terrain` every vertex sits at the height of the nearest road point
/// less `drop`: the road's own landscape, flat across, rising and falling with
/// the road. With it, the land takes its real shape away from the road —
/// held flat within [`SHOULDER_M`], blended in to [`BLEND_END_M`], and
/// shifted to agree with the road's height (see [`LOCAL_FIT_END_M`]).
///
/// Cells whose corners are not all within reach are left out, so the ground
/// ends in a ragged edge the haze hides.
#[cfg(test)]
pub fn ground_mesh(
    road: &[RoadPoint],
    spacing: f32,
    radius: f32,
    drop: f32,
    terrain: Option<Terrain>,
) -> Mesh {
    let grid = Grid {
        spacing,
        radius,
        drop,
    };
    world_mesh(road, grid, terrain, None, &Look::plain(), None)
}

/// The ground, as [`ground_mesh`], and with `world` everything on it: the
/// land coloured by what grows there, coarser ground out to the world's
/// edge, streams, and — into `props_out` — trees and animals standing on the
/// very ground drawn, so none floats or sinks.
pub fn world_mesh(
    road: &[RoadPoint],
    grid: Grid,
    terrain: Option<Terrain>,
    world: Option<&World>,
    look: &Look,
    props_out: Option<&mut Vec<Prop>>,
) -> Mesh {
    let Grid {
        spacing,
        radius,
        drop,
    } = grid;
    let mut mesh = Mesh::default();
    if road.is_empty() || spacing <= 0.0 || radius <= 0.0 {
        return mesh;
    }
    let line = resample(road, RESAMPLE_M);
    let index = NearestIndex::new(&line, radius);
    // How far the road reaches from its line: the track's half width,
    // everywhere.
    let reach = DEFAULT_EXTENT_M;
    let raw_at = |x: f32, z: f32| terrain.and_then(|t| t(x, z));
    // How far the land sits from the road, point by point, and typically.
    let offsets: Vec<Option<f32>> = line
        .iter()
        .map(|p| raw_at(p.pos[0], p.pos[2]).map(|h| p.pos[1] - h))
        .collect();
    let typical = median(offsets.iter().flatten().copied()).unwrap_or(0.0);

    let (mut x_lo, mut x_hi, mut z_lo, mut z_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in road {
        x_lo = x_lo.min(p.pos[0]);
        x_hi = x_hi.max(p.pos[0]);
        z_lo = z_lo.min(p.pos[2]);
        z_hi = z_hi.max(p.pos[2]);
    }

    // Near the road its distance is measured exactly, so a field keeps its
    // verge through a bend.
    let area_at = |x: f32, z: f32| {
        let w = world?;
        match index.nearest(x, z) {
            Some((d2, _)) => w.land_at(x, z, d2.sqrt()),
            None => w.land(x, z),
        }
    };
    let look_at = |x: f32, z: f32| look.biomes.dominant_at(x, z).def();
    let land_at =
        |x: f32, z: f32, rise: f32| ground_colour(look_at(x, z), area_at(x, z), x, z, rise);
    // The ground's height and colour at any point within reach: the grid's
    // vertices, and the spots trees and animals stand on.
    // Round a tight bend on a climb the road is no plane: one leg of it can
    // pass within a grid cell of another metres higher, and a flat triangle
    // laid across the bend from the heights of the nearest points rises over
    // the lower one. So wherever the road within reach turns by more than
    // [`BENT_RAD`], the ground is held under every point of it within reach;
    // along a straighter road, under the nearest point, which the triangles
    // between follow exactly up a steady climb — held lower where the grade
    // changes.
    let hold = SHOULDER_BASE_M.max(spacing * std::f32::consts::SQRT_2 + RESAMPLE_M / 2.0) + reach;
    let grade: Vec<f32> = (0..line.len())
        .map(|k| {
            let (a, b) = (
                &line[k.saturating_sub(1)],
                &line[(k + 1).min(line.len() - 1)],
            );
            let run = b.distance_m - a.distance_m;
            if run > 1e-3 {
                (b.pos[1] - a.pos[1]) / run
            } else {
                0.0
            }
        })
        .collect();
    let heading: Vec<f32> = (0..line.len())
        .map(|k| {
            let a = line[k.saturating_sub(1)].pos;
            let b = line[(k + 1).min(line.len() - 1)].pos;
            (b[2] - a[2]).atan2(b[0] - a[0])
        })
        .collect();
    // How steeply the road rises or falls over its first and last stretch.
    let grade_over = |a: &RoadPoint, b: &RoadPoint| {
        let run = (b.distance_m - a.distance_m).abs();
        if run > 0.5 {
            ((b.pos[1] - a.pos[1]) / run).abs()
        } else {
            0.0
        }
    };
    let end_grade = {
        let n = line.len();
        let k = n.saturating_sub(1).min(4);
        [
            grade_over(&line[0], &line[k]),
            grade_over(&line[n - 1 - k], &line[n - 1]),
        ]
    };
    let under_the_bend = |x: f32, z: f32, i: usize, y: f32| -> f32 {
        let mut y = y;
        // Past either end of the road its last point is the nearest to
        // everything, however the road climbs up to it: there every point
        // within reach holds the ground down, and the ground goes on falling
        // as steeply as the road was.
        let end = i == 0 || i + 1 == line.len();
        if end {
            let dist = (line[i].pos[0] - x).hypot(line[i].pos[2] - z);
            y -= end_grade[usize::from(i > 0)] * dist;
        }
        let mut lowest = f32::MAX;
        let mut bent = false;
        // Where the grade changes within reach — the foot of a steep drop, the
        // top of a climb — the road is no straight line, and a flat triangle
        // laid across the change from its heights either side stands over it
        // by up to a quarter of the change over the triangle's length; the
        // ground is held down by that much.
        let (mut g_lo, mut g_hi) = (f32::MAX, f32::MIN);
        let h0 = heading[i];
        index.for_each_within(x, z, hold, |j| {
            let q = &line[j];
            if (q.pos[0] - x).hypot(q.pos[2] - z) > hold {
                return;
            }
            lowest = lowest.min(q.pos[1] - drop);
            let turn = (heading[j] - h0).rem_euclid(std::f32::consts::TAU);
            bent |= turn.min(std::f32::consts::TAU - turn) > BENT_RAD;
            g_lo = g_lo.min(grade[j]);
            g_hi = g_hi.max(grade[j]);
        });
        if bent || end {
            y.min(lowest)
        } else if g_hi > g_lo {
            y - (g_hi - g_lo) * hold / 4.0
        } else {
            y
        }
    };
    let sample = |x: f32, z: f32| -> Option<(f32, [f32; 4])> {
        let (d2, i) = index.nearest(x, z)?;
        let dist = d2.sqrt();
        let flat = line[i].pos[1] - drop;
        let flat = if dist <= hold {
            under_the_bend(x, z, i, flat)
        } else {
            flat
        };
        let raw = raw_at(x, z);
        Some(match raw {
            Some(raw) if dist > SHOULDER_BASE_M + reach => {
                let near = offsets[i].unwrap_or(typical);
                let shift = lerp(
                    near,
                    typical,
                    smoothstep(BLEND_END_M, LOCAL_FIT_END_M, dist),
                );
                let blended = lerp(
                    flat,
                    raw + shift,
                    smoothstep(SHOULDER_BASE_M + reach, BLEND_END_M, dist),
                );
                // Held within a band round the road that widens with
                // distance, so a coarse cell with one corner on a steep bank
                // turns into an embankment rather than a wall.
                let band = bank_band(dist - reach);
                let y = blended.clamp(flat - band, flat + band);
                (y, land_at(x, z, y - line[i].pos[1]))
            }
            _ => (flat, land_at(x, z, 0.0)),
        })
    };

    // The fine ground near the road. Snapped to multiples of the spacing so
    // two builds of one route produce the same vertices.
    let fine = lay_grid(
        &mut mesh,
        (
            (x_lo - radius, x_hi + radius),
            (z_lo - radius, z_hi + radius),
        ),
        spacing,
        sample,
    );
    let above_road = |x: f32, z: f32, y: f32| world.map(|w| w.above_road(x, z, y));
    restyle_steep(&mut mesh, &fine, spacing, look, &look_at, &above_road);

    // The verge: grass along either side of the road at its own height,
    // sloping out to meet the ground — so the road lies in the land, not on
    // a bank above it. The ground grid is held a little under the road (and
    // further round bends), and the verge covers it.
    let fine_heights: Vec<Option<f32>> = fine
        .slot
        .iter()
        .map(|k| k.map(|k| mesh.vertices[k as usize].pos[1]))
        .collect();
    let drawn = |x: f32, z: f32| fine.height_at(&fine_heights, x, z);
    let verge = Verge::along(&line, &index, reach, &drawn);
    mesh.extend(verge.mesh(&line, &|x, z, rise| land_at(x, z, rise)));
    // Where things stand: on the verge within it, else on the ground drawn.
    let surface = |x: f32, z: f32| -> Option<f32> {
        let (d2, i) = index.nearest(x, z)?;
        verge
            .height(&line, i, x, z, d2.sqrt())
            .or_else(|| drawn(x, z))
            .or_else(|| sample(x, z).map(|(y, _)| y))
    };

    // Coarse ground from where the fine one thins out to the world's edge:
    // the land's own height with the route's typical fit, a little lower
    // where it overlaps the fine ground.
    if let Some(w) = world {
        let reach_far = crate::data::world::REACH_M;
        let far = lay_grid(
            &mut mesh,
            (
                (x_lo - reach_far, x_hi + reach_far),
                (z_lo - reach_far, z_hi + reach_far),
            ),
            FAR_SPACING_M,
            |x, z| {
                let from = w.road_distance(x, z);
                if !(radius - 250.0..=reach_far).contains(&from) {
                    return None;
                }
                let lower = FAR_DROP_M * (1.0 - smoothstep(radius - 100.0, radius + 200.0, from));
                let y = raw_at(x, z)? + typical - lower;
                Some((y, ground_colour(look_at(x, z), w.land(x, z), x, z, 30.0)))
            },
        );
        restyle_steep(&mut mesh, &far, FAR_SPACING_M, look, &look_at, &above_road);
    }

    let Some(world) = world else {
        return mesh;
    };
    // How far a place is from the road's edge, as far as the fine ground
    // reaches; further is "far".
    let from_edge = |x: f32, z: f32| -> Option<f32> {
        let (d2, _) = index.nearest(x, z)?;
        Some((d2.sqrt() - reach).max(0.0))
    };
    // Clear by the stream's own width too, and by half a step of its line,
    // which may cut a corner between two of its points.
    let stream_clear = STREAM_CLEAR_M + reach + STREAM_HALF_M + 6.0;
    let from_line = |x: f32, z: f32| index.nearest(x, z).map_or(f32::MAX, |(d2, _)| d2.sqrt());
    // Brooks only where the biome has them, by where each starts; ditches
    // wherever it digs them.
    let mut streams = world.streams(road, STREAM_EVERY_M, stream_clear, from_line);
    streams.retain(|s| {
        s.first().is_some_and(|&(x, z)| {
            world.biome_at(x, z).def().land.water == biome::WaterKind::Streams
        })
    });
    streams.extend(world.ditches(road, stream_clear, from_line));
    let ground = surface;
    for s in &streams {
        mesh.extend(stream_mesh(s, &ground, &look_at));
    }
    // Stream points, bucketed, for "is there a stream here".
    let mut wet: std::collections::HashMap<(i64, i64), Vec<(f32, f32)>> =
        std::collections::HashMap::new();
    for &(x, z) in streams.iter().flatten() {
        wet.entry(((x / 20.0).floor() as i64, (z / 20.0).floor() as i64))
            .or_default()
            .push((x, z));
    }
    let taken = |x: f32, z: f32| {
        let (kx, kz) = ((x / 20.0).floor() as i64, (z / 20.0).floor() as i64);
        (-1..=1).any(|dx| {
            (-1..=1).any(|dz| {
                wet.get(&(kx + dx, kz + dz))
                    .is_some_and(|ps| ps.iter().any(|&(px, pz)| (px - x).hypot(pz - z) < 8.0))
            })
        })
    };

    if let Some(out) = props_out {
        let far_from = |x: f32, z: f32| from_edge(x, z).unwrap_or(f32::MAX);
        let toward_road = |x: f32, z: f32| {
            index.nearest(x, z).map_or(0.0, |(_, i)| {
                let p = line[i].pos;
                (p[2] - z).atan2(p[0] - x)
            })
        };
        let dressing_margin = dressing::MAX_REACH_M + reach;
        // Houses, landmarks and the rest first: what claims ground keeps
        // the trees and the herds off it.
        let dressed = dressing::place(
            road,
            &dressing::Land {
                seed: world.seed(),
                bounds: (
                    (x_lo - dressing_margin, x_hi + dressing_margin),
                    (z_lo - dressing_margin, z_hi + dressing_margin),
                ),
                from_road: &far_from,
                area: &area_at,
                ground: &ground,
                taken: &taken,
                biome: &|x, z| world.biome_at(x, z),
                toward_road: &toward_road,
            },
        );
        let taken = |x: f32, z: f32| taken(x, z) || dressed.claimed(x, z);
        let near = |x: f32, z: f32| from_edge(x, z).filter(|d| *d <= trees::REACH_M);
        let biome_at = |x: f32, z: f32| look_at(x, z);
        let mut rows = world.poplar_rows(road, ROW_EVERY_M, ROW_NEAR_M + reach);
        rows.retain(|r| {
            r.first()
                .is_some_and(|&(x, z)| world.biome_at(x, z).def().land.poplar_rows)
        });
        let margin = trees::REACH_M + reach;
        let mut props = trees::scatter(&trees::Land {
            bounds: (
                (x_lo - margin, x_hi + margin),
                (z_lo - margin, z_hi + margin),
            ),
            from_road: &near,
            area: &area_at,
            ground: &ground,
            above_road: &|x, z, y| world.above_road(x, z, y),
            taken: &taken,
            biome: &biome_at,
            rows: &rows,
            lone: &[],
            thin_from_m: look.thin_from_m,
        });
        props.extend(animals::place(
            road,
            &animals::Land {
                seed: world.seed(),
                from_road: &far_from,
                area: &area_at,
                ground: &ground,
                taken: &taken,
                biome: &biome_at,
            },
        ));
        props.extend(dressed.props);
        *out = props;
    }
    mesh
}

/// Rows of the verge across each side of the road, from its edge out.
const VERGE_ROWS: usize = 6;
/// How far from the road's line the verge reaches, at most, in metres: past
/// where the ground is held down under the road.
const VERGE_OUT_M: f32 = 36.0;
/// Beside the road's edge the verge lies level with it this far.
const VERGE_FLAT_M: f32 = 1.5;
/// The verge lies this far under the road's edge, so the road wins the depth
/// test without standing up out of the grass.
const VERGE_SINK_M: f32 = 0.02;
/// And at least this far over the ground grid it covers.
const VERGE_OVER_M: f32 = 0.03;

/// The grass either side of the road: at the road's own height beside it,
/// sloping out to meet the ground grid. It stops halfway to any other
/// stretch of road — the inside of a bend, the other leg of a hairpin — so
/// it never lies over the road elsewhere.
struct Verge {
    /// Per road point, left side then right: each row's distance from the
    /// road's line, and its height.
    offsets: Vec<[[f32; VERGE_ROWS]; 2]>,
    heights: Vec<[[f32; VERGE_ROWS]; 2]>,
    /// Per road point, the unit vector to its right.
    rights: Vec<(f32, f32)>,
}

impl Verge {
    /// The verge along `line`, a road `reach` metres either side of it,
    /// over ground whose drawn height is `drawn`.
    fn along(
        line: &[RoadPoint],
        index: &NearestIndex,
        reach: f32,
        drawn: &dyn Fn(f32, f32) -> Option<f32>,
    ) -> Self {
        let n = line.len();
        let mut offsets = Vec::with_capacity(n);
        let mut heights = Vec::with_capacity(n);
        let mut rights = Vec::with_capacity(n);
        for k in 0..n {
            let (a, b) = (line[k.saturating_sub(1)].pos, line[(k + 1).min(n - 1)].pos);
            let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
            let len = dx.hypot(dz);
            let (hx, hz) = if len > 1e-3 {
                (dx / len, dz / len)
            } else {
                (0.0, -1.0)
            };
            let right = (-hz, hx);
            rights.push(right);
            let p = line[k].pos;
            let at = |side: f32, o: f32| (p[0] + right.0 * side * o, p[2] + right.1 * side * o);
            let top = p[1] - VERGE_SINK_M;
            let mut offs = [[0.0; VERGE_ROWS]; 2];
            let mut ys = [[top; VERGE_ROWS]; 2];
            for (s, side) in [-1.0f32, 1.0].into_iter().enumerate() {
                // Out as far as this stretch of road is still the nearest.
                let mut width = reach + 0.5;
                let mut o = reach + 1.0;
                while o <= VERGE_OUT_M {
                    let (x, z) = at(side, o);
                    match index.nearest(x, z) {
                        Some((d2, _)) if d2.sqrt() >= o - 1.0 => width = o,
                        _ => break,
                    }
                    o += 2.0;
                }
                let flat = reach + VERGE_FLAT_M.min(0.4 * (width - reach));
                let (ex, ez) = at(side, width);
                let end = drawn(ex, ez).unwrap_or(top);
                for m in 0..VERGE_ROWS {
                    let off = match m {
                        0 => reach,
                        1 => flat,
                        _ => flat + (width - flat) * (m - 1) as f32 / (VERGE_ROWS - 2) as f32,
                    };
                    offs[s][m] = off;
                    ys[s][m] = if m <= 1 {
                        top
                    } else if m + 1 == VERGE_ROWS {
                        end
                    } else {
                        let t = (off - flat) / (width - flat).max(1e-3);
                        let (x, z) = at(side, off);
                        let under = drawn(x, z).map_or(f32::MIN, |g| g + VERGE_OVER_M);
                        lerp(top, end, t).max(under)
                    };
                }
            }
            offsets.push(offs);
            heights.push(ys);
        }
        Self {
            offsets,
            heights,
            rights,
        }
    }

    /// The verge as triangles, coloured by `colour(x, z, rise)` — rise
    /// above the road.
    fn mesh(&self, line: &[RoadPoint], colour: &dyn Fn(f32, f32, f32) -> [f32; 4]) -> Mesh {
        let mut mesh = Mesh::default();
        for (s, side) in [-1.0f32, 1.0].into_iter().enumerate() {
            let base = mesh.vertices.len() as u32;
            for (k, p) in line.iter().enumerate() {
                let r = self.rights[k];
                for m in 0..VERGE_ROWS {
                    let o = side * self.offsets[k][s][m];
                    let (x, z) = (p.pos[0] + r.0 * o, p.pos[2] + r.1 * o);
                    let y = self.heights[k][s][m];
                    mesh.vertices.push(Vertex {
                        pos: [x, y, z],
                        colour: colour(x, z, y - p.pos[1]),
                    });
                }
            }
            let rows = VERGE_ROWS as u32;
            for k in 0..line.len().saturating_sub(1) as u32 {
                for m in 0..rows - 1 {
                    let a = base + k * rows + m;
                    let b = a + rows;
                    mesh.indices
                        .extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
                }
            }
        }
        mesh
    }

    /// The verge's height at `(x, z)`, `dist` from road point `i` (the
    /// nearest), or `None` past its outer edge.
    fn height(&self, line: &[RoadPoint], i: usize, x: f32, z: f32, dist: f32) -> Option<f32> {
        let p = line.get(i)?.pos;
        let r = self.rights.get(i)?;
        let s = usize::from((x - p[0]) * r.0 + (z - p[2]) * r.1 >= 0.0);
        let (offs, ys) = (&self.offsets[i][s], &self.heights[i][s]);
        if dist > offs[VERGE_ROWS - 1] {
            return None;
        }
        if dist <= offs[0] {
            return Some(ys[0]);
        }
        let m = (1..VERGE_ROWS).find(|&m| dist <= offs[m])?;
        let t = (dist - offs[m - 1]) / (offs[m] - offs[m - 1]).max(1e-3);
        Some(lerp(ys[m - 1], ys[m], t))
    }
}

/// Lay a grid of ground over `bounds` at `spacing`, its vertices where
/// `sample` has ground, and two triangles in every cell whose four corners
/// all do. Returns the grid's layout for [`restyle_steep`].
fn lay_grid(
    mesh: &mut Mesh,
    ((x_lo, x_hi), (z_lo, z_hi)): ((f32, f32), (f32, f32)),
    spacing: f32,
    sample: impl Fn(f32, f32) -> Option<(f32, [f32; 4])>,
) -> GridSlots {
    let i_lo = (x_lo / spacing).floor() as i64;
    let i_hi = (x_hi / spacing).ceil() as i64;
    let j_lo = (z_lo / spacing).floor() as i64;
    let j_hi = (z_hi / spacing).ceil() as i64;
    let cols = (i_hi - i_lo + 1).max(0) as usize;
    let rows = (j_hi - j_lo + 1).max(0) as usize;
    let mut slot: Vec<Option<u32>> = vec![None; cols * rows];
    for r in 0..rows {
        for c in 0..cols {
            let x = (i_lo + c as i64) as f32 * spacing;
            let z = (j_lo + r as i64) as f32 * spacing;
            let Some((y, colour)) = sample(x, z) else {
                continue;
            };
            slot[r * cols + c] = Some(mesh.vertices.len() as u32);
            mesh.vertices.push(Vertex {
                pos: [x, y, z],
                colour,
            });
        }
    }
    for r in 0..rows.saturating_sub(1) {
        for c in 0..cols.saturating_sub(1) {
            let corners = (
                slot[r * cols + c],
                slot[r * cols + c + 1],
                slot[(r + 1) * cols + c],
                slot[(r + 1) * cols + c + 1],
            );
            if let (Some(a), Some(b), Some(d), Some(e)) = corners {
                mesh.indices.extend_from_slice(&[a, d, b, b, d, e]);
            }
        }
    }
    GridSlots {
        slot,
        cols,
        rows,
        i_lo,
        j_lo,
        spacing,
    }
}

/// Which vertex, if any, stands at each point of a grid.
struct GridSlots {
    slot: Vec<Option<u32>>,
    cols: usize,
    rows: usize,
    /// The grid's first column and row, in multiples of the spacing.
    i_lo: i64,
    j_lo: i64,
    spacing: f32,
}

impl GridSlots {
    /// The height of the drawn ground at `(x, z)`: on the very triangle
    /// [`lay_grid`] drew there. `None` off the grid, or in a cell it left out.
    fn height_at(&self, heights: &[Option<f32>], x: f32, z: f32) -> Option<f32> {
        let fx = x / self.spacing - self.i_lo as f32;
        let fz = z / self.spacing - self.j_lo as f32;
        if !(fx >= 0.0 && fz >= 0.0) {
            return None;
        }
        let (c, r) = (fx.floor() as usize, fz.floor() as usize);
        if c + 1 >= self.cols || r + 1 >= self.rows {
            return None;
        }
        let (u, v) = (fx - c as f32, fz - r as f32);
        let y = |r: usize, c: usize| -> Option<f32> { heights[r * self.cols + c] };
        let (a, b, d, e) = (y(r, c)?, y(r, c + 1)?, y(r + 1, c)?, y(r + 1, c + 1)?);
        // Split as `lay_grid` splits it: (a, d, b) and (b, d, e).
        Some(if u + v <= 1.0 {
            a + u * (b - a) + v * (d - a)
        } else {
            e + (1.0 - u) * (d - e) + (1.0 - v) * (b - e)
        })
    }
}

/// Steep ground is rock, and ground above the snow line snow, whatever the
/// biome grows lower down.
fn restyle_steep(
    mesh: &mut Mesh,
    grid: &GridSlots,
    spacing: f32,
    look: &Look,
    look_at: &dyn Fn(f32, f32) -> &'static BiomeDef,
    above_road: &dyn Fn(f32, f32, f32) -> Option<f32>,
) {
    let GridSlots {
        slot, cols, rows, ..
    } = grid;
    let (cols, rows) = (*cols, *rows);
    let at = |r: usize, c: usize| slot[r * cols + c].map(|v| mesh.vertices[v as usize].pos[1]);
    let mut restyle = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let Some(v) = slot[r * cols + c] else {
                continue;
            };
            let vertex = &mesh.vertices[v as usize];
            if vertex.colour[3] != MATERIAL_GROUND {
                continue;
            }
            let y = vertex.pos[1];
            let neighbours = [
                (r > 0).then(|| at(r - 1, c)).flatten(),
                (r + 1 < rows).then(|| at(r + 1, c)).flatten(),
                (c > 0).then(|| at(r, c - 1)).flatten(),
                (c + 1 < cols).then(|| at(r, c + 1)).flatten(),
            ];
            let slope = neighbours
                .iter()
                .flatten()
                .map(|n| (n - y).abs() / spacing)
                .fold(0.0f32, f32::max);
            let (x, z) = (vertex.pos[0], vertex.pos[2]);
            let def = look_at(x, z);
            let p = &def.palette;
            let bare = def.land.bare_above_m.is_some_and(|bare| {
                let ragged = (biome::unit(biome::seed_at(x, z, 1.0, 0xBA2E)) - 0.5)
                    * 2.0
                    * biome::TREE_LINE_SPREAD_M;
                above_road(x, z, y).is_some_and(|up| up > bare + ragged)
            });
            if y > look.snow_line_m {
                restyle.push((v, p.snow));
            } else if slope > ROCK_SLOPE || bare {
                restyle.push((v, p.rock));
            }
        }
    }
    for (v, rgb) in restyle {
        let c = &mut mesh.vertices[v as usize].colour;
        c[..3].copy_from_slice(&rgb);
    }
}

/// A stream as a flat ribbon of water lying on the ground, broken wherever
/// the ground runs out under it.
fn stream_mesh(
    line: &[(f32, f32)],
    ground: &dyn Fn(f32, f32) -> Option<f32>,
    look_at: &dyn Fn(f32, f32) -> &'static BiomeDef,
) -> Mesh {
    let mut mesh = Mesh::default();
    let n = line.len();
    let mut last: Option<u32> = None;
    for k in 0..n {
        let (x, z) = line[k];
        let (ax, az) = line[k.saturating_sub(1)];
        let (bx, bz) = line[(k + 1).min(n - 1)];
        let (dx, dz) = (bx - ax, bz - az);
        let len = dx.hypot(dz);
        let Some(y) = ground(x, z).filter(|_| len > 1e-3) else {
            last = None;
            continue;
        };
        let (rx, rz) = (-dz / len * STREAM_HALF_M, dx / len * STREAM_HALF_M);
        let w = look_at(x, z).palette.water;
        let colour = [w[0], w[1], w[2], MATERIAL_WATER];
        let base = mesh.vertices.len() as u32;
        for side in [-1.0, 1.0] {
            mesh.vertices.push(Vertex {
                pos: [x + rx * side, y + STREAM_LIFT_M, z + rz * side],
                colour,
            });
        }
        if let Some(prev) = last {
            mesh.indices
                .extend_from_slice(&[prev, base, prev + 1, prev + 1, base, base + 1]);
        }
        last = Some(base);
    }
    mesh
}

/// The centreline with a point at least every `step` metres, so a distance to
/// the nearest point is within `step / 2` of the distance to the road.
fn resample(road: &[RoadPoint], step: f32) -> Vec<RoadPoint> {
    let mut out = Vec::with_capacity(road.len());
    for pair in road.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        out.push(a);
        let len = ((b.pos[0] - a.pos[0]).powi(2) + (b.pos[2] - a.pos[2]).powi(2)).sqrt();
        let extra = (len / step).ceil() as usize;
        for k in 1..extra {
            let t = k as f32 / extra as f32;
            out.push(RoadPoint {
                pos: lerp3(a.pos, b.pos, t),
                distance_m: a.distance_m + (b.distance_m - a.distance_m) * t,
            });
        }
    }
    if let Some(last) = road.last() {
        out.push(*last);
    }
    out
}

/// How far the road is drawn on behind the start and past the finish, in
/// metres: the chase camera starts behind the first point, and the finish
/// looks ahead past the last, and a road ending there is the edge of the
/// world square across the view. Past the finish, far enough for the haze.
const LEAD_IN_M: f32 = 120.0;
const LEAD_OUT_M: f32 = 400.0;
const LEAD_STEP_M: f32 = 5.0;
/// A lead stops this far short of any other part of the route, so it never
/// runs across the road ridden — a loop, a lollipop, a finish beside the
/// start.
const LEAD_CLEAR_M: f32 = SHOULDER_BASE_M + DEFAULT_EXTENT_M;
/// The road's direction at either end, taken over this much of it.
const LEAD_AIM_M: f32 = 10.0;

/// `road` with a stretch drawn on straight and level beyond either end, at
/// distances before 0 and past the last. Scenery only: the rider, camera and
/// ride all go by the route's own `road`, which this leaves as it is.
fn with_leads(road: &[RoadPoint]) -> Vec<RoadPoint> {
    let (Some(&first), Some(&last)) = (road.first(), road.last()) else {
        return road.to_vec();
    };
    if last.distance_m - first.distance_m < 1.0 {
        return road.to_vec();
    }
    let lead = |end: RoadPoint, inward: f32, length: f32| -> Vec<RoadPoint> {
        let aim = (end.distance_m + inward * LEAD_AIM_M).clamp(first.distance_m, last.distance_m);
        let Some(inner) = position_at(road, aim) else {
            return Vec::new();
        };
        let Some(dir) = normalise_xz(sub3(end.pos, inner)) else {
            return Vec::new();
        };
        // The route near this end is where the lead starts from; anything
        // further along it is in the way.
        let near_end = 2.0 * LEAD_CLEAR_M;
        let clear = |x: f32, z: f32| {
            road.iter().all(|q| {
                (q.distance_m - end.distance_m).abs() < near_end
                    || (q.pos[0] - x).hypot(q.pos[2] - z) >= LEAD_CLEAR_M
            })
        };
        let mut out = Vec::new();
        let mut s = LEAD_STEP_M;
        while s <= length + 1e-3 {
            let (x, z) = (end.pos[0] + dir[0] * s, end.pos[2] + dir[2] * s);
            if !clear(x, z) {
                break;
            }
            out.push(RoadPoint {
                pos: [x, end.pos[1], z],
                distance_m: end.distance_m - inward * s,
            });
            s += LEAD_STEP_M;
        }
        out
    };
    let mut before = lead(first, 1.0, LEAD_IN_M);
    before.reverse();
    let after = lead(last, -1.0, LEAD_OUT_M);
    let mut out = before;
    out.extend_from_slice(road);
    out.extend(after);
    out
}

fn median(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut v: Vec<f32> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 0 at or before `lo`, 1 at or past `hi`, eased between.
fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Road points bucketed on a grid, for "nearest road point within reach"
/// without testing every point for every ground vertex.
///
/// A ground grid has up to a million vertices, most of them far from the road,
/// and searching outwards from each one cost a minute in a debug build — the
/// rider saw only sky. So the search is done once per bucket instead: each
/// bucket keeps the few road points that could be nearest to *anywhere* in it,
/// and a vertex only measures those.
struct NearestIndex<'a> {
    road: &'a [RoadPoint],
    cell: f32,
    reach: f32,
    buckets: std::collections::HashMap<(i64, i64), Vec<usize>>,
    /// Per bucket, every road point that can be nearest to some position in
    /// it — see [`NearestIndex::candidates`]. Filled as buckets are visited.
    candidates: CandidateCache,
}

/// A bucket's place in the grid.
type BucketKey = (i64, i64);
/// Per-bucket candidate lists, shared out as cheap clones.
type CandidateCache =
    std::cell::RefCell<std::collections::HashMap<BucketKey, std::rc::Rc<[usize]>>>;

impl<'a> NearestIndex<'a> {
    /// Bucket width, in metres.
    const CELL_M: f32 = 100.0;

    fn new(road: &'a [RoadPoint], reach: f32) -> Self {
        let cell = Self::CELL_M;
        let mut buckets: std::collections::HashMap<(i64, i64), Vec<usize>> =
            std::collections::HashMap::new();
        for (i, p) in road.iter().enumerate() {
            buckets
                .entry(Self::key(cell, p.pos[0], p.pos[2]))
                .or_default()
                .push(i);
        }
        Self {
            road,
            cell,
            reach,
            buckets,
            candidates: Default::default(),
        }
    }

    fn key(cell: f32, x: f32, z: f32) -> (i64, i64) {
        ((x / cell).floor() as i64, (z / cell).floor() as i64)
    }

    fn d2(&self, i: usize, x: f32, z: f32) -> f32 {
        let p = self.road[i].pos;
        (p[0] - x).powi(2) + (p[2] - z).powi(2)
    }

    /// Calls `visit` for every road point in the buckets within `limit` of
    /// the bucket holding `(x, z)`.
    fn for_each_within(&self, x: f32, z: f32, limit: f32, mut visit: impl FnMut(usize)) {
        let (kx, kz) = Self::key(self.cell, x, z);
        let rings = (limit / self.cell).ceil() as i64 + 1;
        for dx in -rings..=rings {
            for dz in -rings..=rings {
                if let Some(ids) = self.buckets.get(&(kx + dx, kz + dz)) {
                    ids.iter().for_each(|&i| visit(i));
                }
            }
        }
    }

    /// Squared distance to the nearest road point within `limit` of `(x, z)`,
    /// searching rings of buckets outwards and stopping once no point in the
    /// next ring could beat the best found: any point `k` rings out is at
    /// least `(k − 1)` cells away.
    fn nearest_by_rings(&self, x: f32, z: f32, limit: f32) -> Option<f32> {
        let (kx, kz) = Self::key(self.cell, x, z);
        let rings = (limit / self.cell).ceil() as i64 + 1;
        let mut best: Option<f32> = None;
        for k in 0..=rings {
            if let Some(b) = best {
                let floor = (k - 1).max(0) as f32 * self.cell;
                if floor * floor > b {
                    break;
                }
            }
            for dx in -k..=k {
                for dz in -k..=k {
                    if dx.abs() != k && dz.abs() != k {
                        continue; // inside the ring, already searched
                    }
                    let Some(ids) = self.buckets.get(&(kx + dx, kz + dz)) else {
                        continue;
                    };
                    for &i in ids {
                        let d2 = self.d2(i, x, z);
                        if best.is_none_or(|b| d2 < b) {
                            best = Some(d2);
                        }
                    }
                }
            }
        }
        best.filter(|d2| *d2 <= limit * limit)
    }

    /// Every road point that can be the nearest one to some position in the
    /// bucket `key`.
    ///
    /// With `c` the bucket's centre, `h` its half-diagonal and `m` the nearest
    /// road point to `c`: for any `p` in the bucket, its nearest point `q`
    /// has `|p − q| ≤ |p − m| ≤ |c − m| + h`, so `|c − q| ≤ |c − m| + 2h`.
    /// Keeping every point that close to the centre therefore keeps the
    /// nearest one of every position in the bucket — exactly, not roughly.
    fn candidates(&self, key: (i64, i64)) -> std::rc::Rc<[usize]> {
        if let Some(list) = self.candidates.borrow().get(&key) {
            return list.clone();
        }
        let half_diagonal = self.cell * std::f32::consts::SQRT_2 / 2.0;
        let (cx, cz) = (
            (key.0 as f32 + 0.5) * self.cell,
            (key.1 as f32 + 0.5) * self.cell,
        );
        let list: std::rc::Rc<[usize]> =
            match self.nearest_by_rings(cx, cz, self.reach + half_diagonal) {
                None => std::rc::Rc::from([]),
                Some(d2) => {
                    let keep = d2.sqrt() + 2.0 * half_diagonal;
                    let mut ids = Vec::new();
                    self.for_each_within(cx, cz, keep, |i| {
                        if self.d2(i, cx, cz) <= keep * keep {
                            ids.push(i);
                        }
                    });
                    ids.into()
                }
            };
        self.candidates.borrow_mut().insert(key, list.clone());
        list
    }

    /// Squared distance to, and index of, the nearest road point within reach
    /// of `(x, z)`.
    fn nearest(&self, x: f32, z: f32) -> Option<(f32, usize)> {
        let list = self.candidates(Self::key(self.cell, x, z));
        let mut best: Option<(f32, usize)> = None;
        for &i in list.iter() {
            let d2 = self.d2(i, x, z);
            if best.is_none_or(|(b, _)| d2 < b) {
                best = Some((d2, i));
            }
        }
        best.filter(|(d2, _)| *d2 <= self.reach * self.reach)
    }
}

/// A 4×4 matrix, column-major, as GL reads it.
pub type Mat4 = [f32; 16];

/// Perspective projection with a vertical field of view of `fovy_deg`.
/// An `aspect` of zero or less (a widget not yet allocated) is treated as 1.
#[rustfmt::skip]
pub fn perspective(fovy_deg: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let aspect = if aspect > 0.0 { aspect } else { 1.0 };
    let f = 1.0 / (fovy_deg.to_radians() / 2.0).tan();
    let nf = 1.0 / (near - far);
    [
        f / aspect, 0.0, 0.0, 0.0,
        0.0, f, 0.0, 0.0,
        0.0, 0.0, (far + near) * nf, -1.0,
        0.0, 0.0, 2.0 * far * near * nf, 0.0,
    ]
}

/// View matrix for a camera at `eye` looking at `target`, with y up.
#[rustfmt::skip]
pub fn look_at(eye: [f32; 3], target: [f32; 3]) -> Mat4 {
    let f = normalise3(sub3(target, eye)).unwrap_or([0.0, 0.0, -1.0]);
    // Right = forward × up, with up = +y.
    let s = normalise3([-f[2], 0.0, f[0]]).unwrap_or([1.0, 0.0, 0.0]);
    let u = [
        s[1] * f[2] - s[2] * f[1],
        s[2] * f[0] - s[0] * f[2],
        s[0] * f[1] - s[1] * f[0],
    ];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    [
        s[0], u[0], -f[0], 0.0,
        s[1], u[1], -f[1], 0.0,
        s[2], u[2], -f[2], 0.0,
        -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
    ]
}

/// The identity: a model left where it is.
pub const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

/// Move by `t`.
pub fn translation(t: [f32; 3]) -> Mat4 {
    let mut m = IDENTITY;
    m[12] = t[0];
    m[13] = t[1];
    m[14] = t[2];
    m
}

/// Turn about the vertical so +x faces (cos `a`, 0, sin `a`) — the props'
/// and the rider's heading, as the shaders turn them.
pub fn yaw(a: f32) -> Mat4 {
    let (s, c) = a.sin_cos();
    [
        c, 0.0, s, 0.0, 0.0, 1.0, 0.0, 0.0, -s, 0.0, c, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// Tip +x up towards +y by `a`: nose up on a climb, a leg swung forward.
pub fn pitch(a: f32) -> Mat4 {
    let (s, c) = a.sin_cos();
    [
        c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// Tip +y over towards +z by `a`: rider space leaning to the right.
pub fn roll(a: f32) -> Mat4 {
    let (s, c) = a.sin_cos();
    [
        1.0, 0.0, 0.0, 0.0, 0.0, c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// The rider's model: rider space set down at the pose — turned to its
/// heading, tipped with the road and leaning into the bend about the line
/// where the wheels touch it.
pub fn rider_model(r: &RiderPose) -> Mat4 {
    mul4(
        &translation(r.pos),
        &mul4(&yaw(r.yaw), &mul4(&pitch(r.pitch), &roll(r.lean))),
    )
}

/// `a × b` for column-major matrices.
pub fn mul4(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    out
}

/// `m × v` for a point (w = 1), returning all four components.
#[cfg(test)]
pub fn transform(m: &Mat4, v: [f32; 3]) -> [f32; 4] {
    let mut out = [0.0; 4];
    for (r, o) in out.iter_mut().enumerate() {
        *o = m[r] * v[0] + m[4 + r] * v[1] + m[8 + r] * v[2] + m[12 + r];
    }
    out
}

fn normalise3(v: [f32; 3]) -> Option<[f32; 3]> {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (len > 1e-6).then(|| [v[0] / len, v[1] / len, v[2] / len])
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// `v` flattened onto the ground plane and scaled to length 1, or `None` when
/// it has no horizontal extent to speak of.
fn normalise_xz(v: [f32; 3]) -> Option<[f32; 3]> {
    let len = (v[0] * v[0] + v[2] * v[2]).sqrt();
    (len > 1e-3).then(|| [v[0] / len, 0.0, v[2] / len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    /// A straight road due north from (52.0, 6.0), one point every `step` m,
    /// climbing `rise` metres per point.
    fn straight_route(points: usize, step: f64, rise: f32) -> Route {
        let frame = LocalFrame::at(52.0, 6.0);
        let pts: Vec<RoutePoint> = (0..points)
            .map(|i| {
                let (lat, lng) = frame.to_geo(0.0, i as f64 * step);
                RoutePoint {
                    lat,
                    lng,
                    elevation_m: 10.0 + rise * i as f32,
                    distance_m: (i as f64 * step) as f32,
                    gradient: 0.0,
                }
            })
            .collect();
        Route {
            name: "Straight".into(),
            total_distance_m: pts.last().map(|p| p.distance_m).unwrap_or(0.0),
            total_gain_m: 0.0,
            points: pts,
        }
    }

    /// The route's own points laid due north (−z) along x = 0, at their own
    /// heights and distances: a plain road for testing what stands on one.
    fn road_of(route: &Route) -> Vec<RoadPoint> {
        route
            .points
            .iter()
            .map(|p| RoadPoint {
                pos: [0.0, p.elevation_m, -p.distance_m],
                distance_m: p.distance_m,
            })
            .collect()
    }

    // ── Local frame ───────────────────────────────────────────────────────────

    #[test]
    fn should_round_trip_a_point_fifty_km_out_to_within_a_millimetre() {
        let frame = LocalFrame::at(40.94, 29.14);
        let (lat, lng) = frame.to_geo(50_000.0, -50_000.0);
        let (e, n) = frame.to_local(lat, lng);
        assert!((e - 50_000.0).abs() < 1e-3, "east {e}");
        assert!((n + 50_000.0).abs() < 1e-3, "north {n}");
    }

    #[test]
    fn should_put_a_degree_of_latitude_at_the_physics_earth_radius() {
        // The same sphere the ride's distance is measured on.
        let frame = LocalFrame::at(52.0, 6.0);
        let (_, n) = frame.to_local(53.0, 6.0);
        let expected = 6_371_000.0 * std::f64::consts::PI / 180.0;
        assert!((n - expected).abs() < 1e-6, "got {n}");
    }

    /// Rule 4 of the stylized-route skill: 1 km ridden is 1 km of road. A short
    /// segment measured in the scene is the length the ride measures for it,
    /// east-west as well as north-south, in both of the rider's regions and
    /// kilometres from the frame's origin.
    #[test]
    fn should_measure_a_segment_as_the_ride_measures_it() {
        for &(lat0, lng0) in &[(51.9, 6.3), (40.95, 29.14)] {
            let frame = LocalFrame::at(lat0, lng0);
            for &(dlat, dlng) in &[(0.0, 0.0001), (0.0001, 0.0), (0.00007, 0.00007)] {
                for &(off_lat, off_lng) in &[(0.0, 0.0), (0.12, -0.15), (-0.09, 0.2)] {
                    let (a_lat, a_lng) = (lat0 + off_lat, lng0 + off_lng);
                    let (b_lat, b_lng) = (a_lat + dlat, a_lng + dlng);
                    let (ax, az) = frame.to_local(a_lat, a_lng);
                    let (bx, bz) = frame.to_local(b_lat, b_lng);
                    let scene = (bx - ax).hypot(bz - az);
                    let ride = f64::from(crate::data::route::haversine(a_lat, a_lng, b_lat, b_lng));
                    assert!(
                        (scene / ride - 1.0).abs() < 2e-4,
                        "at {a_lat},{a_lng} going {dlat},{dlng}: scene {scene} ride {ride}"
                    );
                }
            }
        }
    }

    #[test]
    fn should_shrink_a_degree_of_longitude_with_latitude() {
        // At 60° a degree of longitude is about half what it is at the equator.
        let (e60, _) = LocalFrame::at(60.0, 0.0).to_local(60.0, 1.0);
        let (e0, _) = LocalFrame::at(0.0, 0.0).to_local(0.0, 1.0);
        assert!((e60 / e0 - 0.5).abs() < 0.01, "ratio {}", e60 / e0);
    }

    #[test]
    fn should_have_no_frame_for_an_empty_route() {
        let route = straight_route(0, 10.0, 0.0);
        assert!(LocalFrame::around(&route).is_none());
        assert!(CourseScene::build(&route, Scenery::Countryside).is_none());
    }

    #[test]
    fn should_not_build_a_scene_from_a_single_point() {
        assert!(CourseScene::build(&straight_route(1, 10.0, 0.0), Scenery::Countryside).is_none());
    }

    /// Whether the scene draws road surface within a metre of `(x, z)`, at
    /// height `y`.
    fn road_drawn_at(scene: &CourseScene, x: f32, y: f32, z: f32) -> bool {
        scene.mesh.vertices.iter().any(|v| {
            v.colour[3] == MATERIAL_ROAD
                && (v.pos[0] - x).hypot(v.pos[2] - z) < 3.0
                && (v.pos[1] - y).abs() < 0.05
        })
    }

    #[test]
    fn should_draw_road_before_the_start_and_past_the_finish() {
        // Seen from the chase camera at the start, a road that began at the
        // first GPX point was the edge of the world, square across the view.
        let route = straight_route(31, 10.0, 0.5);
        let scene = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        // The made-up road bends, so each end goes on its own way.
        let (first, last) = (scene.road[0].pos, scene.road[scene.road.len() - 1].pos);
        let into = position_at(&scene.road, LEAD_AIM_M).expect("a road");
        let back = position_at(&scene.road, 300.0 - LEAD_AIM_M).expect("a road");
        let out_of = normalise_xz(sub3(first, into)).expect("a heading");
        let on = normalise_xz(sub3(last, back)).expect("a heading");
        let before = [first[0] + out_of[0] * 40.0, first[2] + out_of[2] * 40.0];
        let past = [last[0] + on[0] * 150.0, last[2] + on[2] * 150.0];
        assert!(
            road_drawn_at(&scene, before[0], first[1], before[1]),
            "no road 40 m before the start"
        );
        assert!(
            road_drawn_at(&scene, past[0], last[1], past[1]),
            "no road 150 m past the finish"
        );
    }

    #[test]
    fn should_bare_the_mountains_high_ground_to_rock() {
        let route = straight_route(301, 10.0, 0.0);
        let scenery = Scenery::Only(biome::Biome::Mountain);
        let scene = CourseScene::build(&route, scenery).expect("a scene");
        let world = World::new(&track::lay(&route));
        let def = biome::Biome::Mountain.def();
        let bare = def.land.bare_above_m.expect("bare ground");
        let rock = def.palette.rock;
        let high: Vec<&Vertex> = scene
            .mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] == MATERIAL_GROUND)
            .filter(|v| {
                let up = world.above_road(v.pos[0], v.pos[2], v.pos[1]);
                up > bare + biome::TREE_LINE_SPREAD_M
            })
            .collect();
        assert!(high.len() > 50, "only {} high ground vertices", high.len());
        for v in high {
            assert!(
                v.colour[..3] == rock || v.colour[..3] == def.palette.snow,
                "high ground at {:?} is {:?}",
                v.pos,
                &v.colour[..3]
            );
        }
    }

    #[test]
    fn should_leave_the_ridden_road_its_own_ends() {
        // The road drawn on beyond the ends is scenery: the rider, camera and
        // sky still go by the route's own road, 0 m to its full length.
        let route = straight_route(31, 10.0, 0.5);
        let scene = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        assert_eq!(scene.road[0].distance_m, 0.0);
        assert_eq!(scene.road[scene.road.len() - 1].distance_m, 300.0);
        assert_eq!(scene.road, track::lay(&route));
    }

    #[test]
    fn should_stop_the_road_past_the_finish_short_of_the_route_itself() {
        // North 300 m, east 200, south 150, then west 100: carried on west,
        // the finish would run across the first leg at x = 0.
        let mut road = Vec::new();
        let mut at = |x: f32, z: f32| {
            let d = road.last().map_or(0.0, |p: &RoadPoint| {
                p.distance_m + (p.pos[0] - x).hypot(p.pos[2] - z)
            });
            road.push(RoadPoint {
                pos: [x, 10.0, z],
                distance_m: d,
            });
        };
        for k in 0..=300 {
            at(0.0, -(k as f32));
        }
        for k in 1..=200 {
            at(k as f32, -300.0);
        }
        for k in 1..=150 {
            at(200.0, -300.0 + k as f32);
        }
        for k in 1..=100 {
            at(200.0 - k as f32, -150.0);
        }
        let drawn = with_leads(&road);
        let added = drawn.iter().filter(|p| p.distance_m > 750.0);
        let mut any = false;
        for p in added {
            any = true;
            assert!(
                p.pos[0] >= LEAD_CLEAR_M,
                "the lead reaches x = {}",
                p.pos[0]
            );
        }
        assert!(any, "no road drawn past the finish at all");
        // The road ridden is all still there, unmoved.
        assert!(drawn.windows(road.len()).any(|w| w == road.as_slice()));
    }

    #[test]
    fn should_draw_the_road_on_level_beyond_the_ends() {
        let road = road_of(&straight_route(11, 10.0, 1.0));
        let drawn = with_leads(&road);
        let (first, last) = (road[0], road[road.len() - 1]);
        assert!(drawn[0].distance_m <= -LEAD_IN_M + LEAD_STEP_M);
        assert!(drawn[drawn.len() - 1].distance_m >= last.distance_m + LEAD_OUT_M - LEAD_STEP_M);
        for p in &drawn {
            if p.distance_m < 0.0 {
                assert_eq!(p.pos[1], first.pos[1]);
                assert_eq!(p.pos[0], 0.0);
                assert!(p.pos[2] > 0.0, "the lead-in is behind the start");
            } else if p.distance_m > last.distance_m {
                assert_eq!(p.pos[1], last.pos[1]);
            }
        }
    }

    // ── The made-up road in the scene ─────────────────────────────────────────

    #[test]
    fn should_keep_the_gpx_elevation_exactly_even_a_spike() {
        // The road's height is the ride's height: no smoothing on the visual
        // side, so a one-point spike in the GPX stays a spike in the road.
        let mut route = straight_route(21, 10.0, 0.0);
        route.points[10].elevation_m += 7.0;
        let scene = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        assert_eq!(position_at(&scene.road, 100.0).expect("a road")[1], 17.0);
        assert_eq!(position_at(&scene.road, 90.0).expect("a road")[1], 10.0);
        assert_eq!(position_at(&scene.road, 95.0).expect("a road")[1], 13.5);
    }

    #[test]
    fn should_put_the_road_at_the_rides_elevation_every_ten_metres() {
        // A climb with GPS noise in the height, as recorded routes have.
        let mut route = straight_route(200, 8.5, 0.6);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.elevation_m += ((i * 7919 % 13) as f32 / 6.0 - 1.0) * 0.8;
        }
        let road = track::lay(&route);
        let mut d = 0.0;
        while d <= route.total_distance_m {
            let on_road = position_at(&road, d).expect("a road")[1];
            let ridden = route.elevation_at(d);
            assert!(
                (on_road - ridden).abs() <= 0.01,
                "at {d} m the road is at {on_road}, the ride at {ridden}"
            );
            d += 10.0;
        }
    }

    // ── Position and camera ───────────────────────────────────────────────────

    #[test]
    fn should_interpolate_between_points() {
        let road = road_of(&straight_route(3, 10.0, 2.0));
        let p = position_at(&road, 5.0).expect("a road");
        let mid = lerp3(road[0].pos, road[1].pos, 0.5);
        assert_eq!(p, mid);
    }

    #[test]
    fn should_clamp_before_the_start_and_after_the_finish() {
        let road = road_of(&straight_route(3, 10.0, 0.0));
        assert_eq!(position_at(&road, -50.0), Some(road[0].pos));
        assert_eq!(position_at(&road, 1e9), Some(road[2].pos));
        assert_eq!(position_at(&road, f32::NAN), Some(road[0].pos));
        assert_eq!(position_at(&[], 10.0), None);
    }

    fn rider_at(road: &[RoadPoint], d: f32) -> RiderPose {
        rider_pose(road, d, 0.0).expect("a road")
    }

    #[test]
    fn should_put_the_wheels_exactly_on_the_road_surface() {
        let road = road_of(&straight_route(50, 10.0, 0.5));
        let rider = rider_at(&road, 123.0);
        let surface = position_at(&road, 123.0).expect("a road");
        assert_eq!(rider.pos[1], surface[1]);
    }

    #[test]
    fn should_face_up_the_road() {
        // Due north is −z: heading (cos, sin) = (0, −1).
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let rider = rider_at(&road, 100.0);
        assert!(
            rider.yaw.cos().abs() < 1e-3 && (rider.yaw.sin() + 1.0).abs() < 1e-3,
            "{rider:?}"
        );
    }

    #[test]
    fn should_follow_from_behind_and_above_looking_past_the_rider() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let rider = rider_at(&road, 200.0);
        let cam = chase_camera(&road, &rider);
        // North is −z: behind is +z.
        assert!(
            (cam.eye[2] - (rider.pos[2] + CHASE_BACK_M)).abs() < 1e-3,
            "{cam:?}"
        );
        assert!((cam.eye[1] - (rider.pos[1] + CHASE_UP_M)).abs() < 1e-4);
        assert!((cam.target[2] - (rider.pos[2] - CHASE_AHEAD_M)).abs() < 1e-3);
        assert!(
            (cam.target[0] - cam.eye[0]).abs() < 1e-3,
            "looking straight down the road"
        );
        // The camera sees the rider: they are between it and where it aims,
        // below its line of sight.
        assert!(
            cam.eye[1] > rider.pos[1] + 1.6,
            "the camera is above the rider's head"
        );
    }

    #[test]
    fn should_tip_the_rider_up_a_climb_and_down_a_descent() {
        for (rise, want) in [
            (0.5f32, (0.05f32).atan()),
            (-0.8, (-0.08f32).atan()),
            (0.0, 0.0),
        ] {
            let road = road_of(&straight_route(80, 10.0, rise));
            let rider = rider_at(&road, 300.0);
            assert!(
                (rider.pitch - want).abs() < 1e-3,
                "{} vs {want}",
                rider.pitch
            );
        }
    }

    #[test]
    fn should_turn_the_cranks_with_the_distance_ridden() {
        let road = road_of(&straight_route(80, 10.0, 0.0));
        assert_eq!(rider_at(&road, 0.0).crank, 0.0);
        assert_eq!(rider_at(&road, 77.0).crank, rider::crank(77.0));
        assert_ne!(rider_at(&road, 77.0).crank, rider_at(&road, 78.0).crank);
    }

    #[test]
    fn should_set_the_rider_model_down_at_the_pose() {
        let rider = RiderPose {
            pos: [10.0, 5.0, -3.0],
            yaw: 0.7,
            pitch: 0.0,
            lean: 0.0,
            crank: 0.0,
            distance_m: 0.0,
        };
        let m = rider_model(&rider);
        let o = transform(&m, [0.0, 0.0, 0.0]);
        assert!(close(o[0], 10.0) && close(o[1], 5.0) && close(o[2], -3.0));
        // Rider space's +x points along the heading.
        let f = transform(&m, [1.0, 0.0, 0.0]);
        assert!(
            close(f[0] - 10.0, 0.7f32.cos()) && close(f[2] + 3.0, 0.7f32.sin()),
            "{f:?}"
        );
        // Up stays up when the road is level.
        let up = transform(&m, [0.0, 1.0, 0.0]);
        assert!(close(up[1], 6.0));
    }

    #[test]
    fn should_tip_forward_up_with_a_positive_pitch() {
        let p = transform(&pitch(0.3), [1.0, 0.0, 0.0]);
        assert!(close(p[0], 0.3f32.cos()) && close(p[1], 0.3f32.sin()));
        // A leg hanging down swings forward (+x) with a positive angle.
        let foot = transform(&pitch(0.3), [0.0, -1.0, 0.0]);
        assert!(foot[0] > 0.0 && foot[1] < 0.0, "{foot:?}");
        let y = transform(&yaw(std::f32::consts::FRAC_PI_2), [1.0, 0.0, 0.0]);
        assert!(close(y[0], 0.0) && close(y[2], 1.0), "{y:?}");
        assert_eq!(transform(&IDENTITY, [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0, 1.0]);
    }

    /// A straight road due east, one point every 10 m.
    fn east_line(points: usize) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [i as f32 * 10.0, 10.0, 0.0],
                distance_m: i as f32 * 10.0,
            })
            .collect()
    }

    #[test]
    fn should_ride_the_right_lane_beside_the_line_whichever_way_it_runs() {
        // The track is centred on the road's line; the rider is in the middle
        // of its right-hand lane, level with where the line puts them.
        for road in [road_of(&straight_route(50, 10.0, 0.0)), east_line(50)] {
            for d in [0.0, 200.0, 490.0, 5_000.0] {
                let rider = rider_at(&road, d);
                let on = position_at(&road, d).expect("a road");
                let (dx, dz) = (rider.pos[0] - on[0], rider.pos[2] - on[2]);
                assert!(
                    (dx.hypot(dz) - RIDER_OFFSET_M).abs() < 1e-3,
                    "at {d}: {rider:?}"
                );
                // To the right: heading × offset points down (y = −1).
                let (hx, hz) = (rider.yaw.cos(), rider.yaw.sin());
                assert!(
                    hz * dx - hx * dz < 0.0,
                    "left of the line at {d}: {rider:?}"
                );
                // Level with the line, not ahead of or behind it.
                assert!((hx * dx + hz * dz).abs() < 1e-2, "at {d}: {rider:?}");
            }
        }
    }

    #[test]
    fn should_keep_facing_forward_at_and_past_the_finish() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        for d in [490.0, 500.0, 10_000.0] {
            let rider = rider_at(&road, d);
            assert!(
                rider.yaw.sin() < -0.99,
                "at {d} m the rider turned: {rider:?}"
            );
        }
    }

    #[test]
    fn should_have_a_heading_on_a_road_of_one_short_step() {
        let road = road_of(&straight_route(2, 5.0, 0.0));
        assert!(rider_at(&road, 0.0).yaw.is_finite());
    }

    #[test]
    fn should_still_face_somewhere_when_every_point_is_the_same() {
        // A GPX of a rider standing still: one place, many points.
        let mut route = straight_route(10, 0.0, 0.0);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.distance_m = i as f32;
        }
        let road = road_of(&route);
        let rider = rider_at(&road, 3.0);
        assert!(rider.yaw.is_finite() && rider.pitch.is_finite());
        let cam = chase_camera(&road, &rider);
        assert!(normalise_xz(sub3(cam.target, cam.eye)).is_some());
    }

    /// North 100 m, a quarter turn of `radius` metres (right if `right`), then
    /// on 100 m; a point every metre of the bend, as the made-up road has.
    fn bend_line(radius: f32, right: bool) -> Vec<RoadPoint> {
        let side = if right { 1.0 } else { -1.0 };
        let mut line: Vec<RoadPoint> = (0..=20)
            .map(|i| RoadPoint {
                pos: [0.0, 0.0, -(i as f32) * 5.0],
                distance_m: i as f32 * 5.0,
            })
            .collect();
        let arc = radius * std::f32::consts::FRAC_PI_2;
        let steps = arc.ceil() as usize;
        for k in 1..=steps {
            let a = k as f32 / steps as f32 * std::f32::consts::FRAC_PI_2;
            line.push(RoadPoint {
                pos: [
                    side * (radius - radius * a.cos()),
                    0.0,
                    -100.0 - radius * a.sin(),
                ],
                distance_m: 100.0 + arc * k as f32 / steps as f32,
            });
        }
        for i in 1..=20 {
            line.push(RoadPoint {
                pos: [side * (radius + i as f32 * 5.0), 0.0, -100.0 - radius],
                distance_m: 100.0 + arc + i as f32 * 5.0,
            });
        }
        line
    }

    /// The flat angle from `a` to `b`, right positive, in (−π, π].
    fn turn_between(a: f32, b: f32) -> f32 {
        let d = (b - a).rem_euclid(std::f32::consts::TAU);
        if d > std::f32::consts::PI {
            d - std::f32::consts::TAU
        } else {
            d
        }
    }

    #[test]
    fn should_face_along_the_road_under_the_wheels_round_a_tight_bend() {
        // Round a 12 m bend the rider faces where the road goes beneath
        // them, not across the bend to a point further on.
        let road = bend_line(12.0, true);
        let arc = 12.0 * std::f32::consts::FRAC_PI_2;
        for k in 1..10 {
            let s = arc * k as f32 / 10.0;
            let rider = rider_at(&road, 100.0 + s);
            // Heading north is −π/2; the bend turns right by s / r.
            let want = -std::f32::consts::FRAC_PI_2 + s / 12.0;
            assert!(
                turn_between(rider.yaw, want).abs() < 3f32.to_radians(),
                "{s} m into the bend facing {} not {want}",
                rider.yaw
            );
        }
    }

    #[test]
    fn should_lean_into_a_bend_as_far_as_the_speed_asks() {
        // tan θ = v² / (g r): 8 m/s round 20 m is about 18°.
        let want = (64.0f32 / (9.81 * 20.0)).atan();
        let right = rider_pose(&bend_line(20.0, true), 115.0, 8.0).expect("a road");
        assert!((right.lean - want).abs() < 0.02, "{} vs {want}", right.lean);
        let left = rider_pose(&bend_line(20.0, false), 115.0, 8.0).expect("a road");
        assert!(
            (left.lean + want).abs() < 0.02,
            "{} vs {}",
            left.lean,
            -want
        );
        // Upright on the straight, standing still, and for a speed that is
        // no speed at all.
        assert_eq!(
            rider_pose(&bend_line(20.0, true), 40.0, 8.0).map(|r| r.lean),
            Some(0.0)
        );
        assert_eq!(
            rider_pose(&bend_line(20.0, true), 115.0, 0.0).map(|r| r.lean),
            Some(0.0)
        );
        for v in [f32::NAN, f32::INFINITY, -8.0] {
            let r = rider_pose(&bend_line(20.0, true), 115.0, v).expect("a road");
            assert_eq!(r.lean, 0.0, "at {v}");
        }
        // Never further over than a rider can go.
        let fast = rider_pose(&bend_line(8.0, true), 106.0, 20.0).expect("a road");
        assert_eq!(fast.lean, MAX_LEAN_RAD);
    }

    #[test]
    fn should_lean_the_rider_model_to_the_right_with_a_positive_lean() {
        let head = transform(&roll(0.3), [0.0, 1.5, 0.0]);
        assert!(
            head[2] > 0.0 && close(head[1], 1.5 * 0.3f32.cos()),
            "{head:?}"
        );
        // About the line the wheels touch the road on: they stay put.
        let wheel = transform(&roll(0.3), [0.5, 0.0, 0.0]);
        assert_eq!(wheel, [0.5, 0.0, 0.0, 1.0]);
        let rider = RiderPose {
            pos: [0.0; 3],
            yaw: -std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            lean: 0.3,
            crank: 0.0,
            distance_m: 0.0,
        };
        // Heading north, leaning right puts the head to the east.
        let head = transform(&rider_model(&rider), [0.0, 1.5, 0.0]);
        assert!(head[0] > 0.4, "{head:?}");
    }

    #[test]
    fn should_swing_the_camera_round_a_bend_smoothly_and_after_the_rider() {
        let road = bend_line(10.0, true);
        let heading = |c: &CameraPose| (c.target[2] - c.eye[2]).atan2(c.target[0] - c.eye[0]);
        let mut last: Option<f32> = None;
        let mut d = 80.0;
        while d < 160.0 {
            let rider = rider_at(&road, d);
            let cam = heading(&chase_camera(&road, &rider));
            if let Some(cam_was) = last {
                let dc = turn_between(cam_was, cam).abs();
                // Half a metre on, the camera never turns more sharply than
                // the road does, and eases into the bend: in its first
                // metres, at under half the road's rate.
                let road_rate = 0.5 / 10.0;
                assert!(dc <= road_rate + 1e-3, "{dc} rad over 0.5 m at {d}");
                if (100.0..103.0).contains(&d) {
                    assert!(dc < road_rate / 2.0, "{dc} rad over 0.5 m at {d}");
                }
            }
            // Never further round than the road just ahead of the rider: it
            // looks a little into a bend, never across it.
            let north = -std::f32::consts::FRAC_PI_2;
            let ahead = direction_at(&road, d + CAMERA_LEAD_M).expect("a road");
            assert!(
                turn_between(north, cam) <= turn_between(north, ahead[2].atan2(ahead[0])) + 1e-3,
                "at {d} the camera looks across the bend"
            );
            // And behind the rider through the bend itself.
            if (105.0..115.0).contains(&d) {
                assert!(
                    turn_between(north, cam) < turn_between(north, rider.yaw),
                    "at {d} the camera is not following"
                );
            }
            last = Some(cam);
            d += 0.5;
        }
        // Well past the bend it looks straight down the road again.
        let rider = rider_at(&road, 160.0);
        let cam = heading(&chase_camera(&road, &rider));
        assert!(turn_between(cam, 0.0).abs() < 1e-3, "{cam}");
    }

    // ── Meshes ────────────────────────────────────────────────────────────────

    #[test]
    fn should_index_only_vertices_that_exist() {
        let scene = CourseScene::build(&straight_route(40, 9.0, 0.3), Scenery::Countryside)
            .expect("a route");
        let n = scene.mesh.vertices.len() as u32;
        assert!(scene.mesh.indices.iter().all(|&i| i < n));
        assert_eq!(scene.mesh.indices.len() % 3, 0);
    }

    /// A hairpin of `radius` metres turning `degrees`, climbing `grade` all the
    /// way: a point every metre of it, as the made-up road has, with 100 m of
    /// straight either side.
    fn climbing_hairpin(radius: f32, degrees: f32, grade: f32) -> Vec<RoadPoint> {
        let mut line = Vec::new();
        let mut push = |x: f32, z: f32| {
            let d = line.last().map_or(0.0, |p: &RoadPoint| {
                p.distance_m + (x - p.pos[0]).hypot(z - p.pos[2])
            });
            line.push(RoadPoint {
                pos: [x, d * grade, z],
                distance_m: d,
            });
        };
        for i in 0..=100 {
            push(0.0, -(i as f32));
        }
        let turn = degrees.to_radians();
        let steps = (radius * turn).ceil() as usize;
        for k in 1..=steps {
            let a = turn * k as f32 / steps as f32;
            push(radius - radius * a.cos(), -100.0 - radius * a.sin());
        }
        let (ex, ez) = (radius - radius * turn.cos(), -100.0 - radius * turn.sin());
        let (hx, hz) = (turn.sin(), -turn.cos());
        for i in 1..=100 {
            push(ex + hx * i as f32, ez + hz * i as f32);
        }
        line
    }

    /// Whether any ground triangle stands above the road's surface: tested
    /// at points across the whole width of the road, every half metre along
    /// it.
    fn ground_over_road(road: &[RoadPoint], ground: &Mesh) -> Option<String> {
        let mut spots = Vec::new();
        let mut d = 0.0;
        let end = road.last().map_or(0.0, |p| p.distance_m);
        while d <= end {
            let p = position_at(road, d).expect("a road");
            let h = direction_at(road, d).expect("a road");
            for off in [-2.6f32, -1.3, 0.0, 1.3, 2.6] {
                spots.push((p[0] - h[2] * off, p[2] + h[0] * off, p[1]));
            }
            d += 0.5;
        }
        for t in ground.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| ground.vertices[t[k] as usize].pos);
            let det = (b[2] - c[2]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[2] - c[2]);
            if det.abs() < 1e-6 {
                continue;
            }
            for &(x, z, y) in &spots {
                let l1 = ((b[2] - c[2]) * (x - c[0]) + (c[0] - b[0]) * (z - c[2])) / det;
                let l2 = ((c[2] - a[2]) * (x - c[0]) + (a[0] - c[0]) * (z - c[2])) / det;
                let l3 = 1.0 - l1 - l2;
                if l1 < 0.0 || l2 < 0.0 || l3 < 0.0 {
                    continue;
                }
                let g = l1 * a[1] + l2 * b[1] + l3 * c[1];
                if g > y + 1e-3 {
                    return Some(format!("ground {g} over the road at {y} at ({x}, {z})"));
                }
            }
        }
        None
    }

    /// The topmost ground drawn over `(x, z)`: what the eye sees there.
    fn top_ground(ground: &Mesh, x: f32, z: f32) -> Option<f32> {
        let mut top: Option<f32> = None;
        for t in ground.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| ground.vertices[t[k] as usize].pos);
            let det = (b[2] - c[2]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[2] - c[2]);
            if det.abs() < 1e-6 {
                continue;
            }
            let l1 = ((b[2] - c[2]) * (x - c[0]) + (c[0] - b[0]) * (z - c[2])) / det;
            let l2 = ((c[2] - a[2]) * (x - c[0]) + (a[0] - c[0]) * (z - c[2])) / det;
            let l3 = 1.0 - l1 - l2;
            if l1 < -1e-5 || l2 < -1e-5 || l3 < -1e-5 {
                continue;
            }
            let g = l1 * a[1] + l2 * b[1] + l3 * c[1];
            top = Some(top.map_or(g, |t: f32| t.max(g)));
        }
        top
    }

    #[test]
    fn should_lay_the_grass_level_with_the_road_edge() {
        // The rider saw every road standing up out of the land. Beside the
        // edge the grass is the road's height, on a climb with GPS noise in
        // it and round a steep hairpin alike.
        let mut route = straight_route(150, 8.5, 0.6);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.elevation_m += ((i * 7919 % 13) as f32 / 6.0 - 1.0) * 0.8;
        }
        for road in [road_of(&route), climbing_hairpin(12.0, 75.0, 0.1)] {
            let ground = ground_mesh(&road, GROUND_SPACING_M, 120.0, GROUND_DROP_M, None);
            let end = road.last().map_or(0.0, |p| p.distance_m);
            let mut d = 40.0;
            while d < end - 40.0 {
                let p = position_at(&road, d).expect("a road");
                let h = direction_at(&road, d).expect("a road");
                for side in [-1.0f32, 1.0] {
                    let o = side * (roadway::TRACK_HALF_WIDTH_M + 0.5);
                    let (x, z) = (p[0] - h[2] * o, p[2] + h[0] * o);
                    let g = top_ground(&ground, x, z).expect("ground beside the road");
                    assert!(
                        (p[1] - g) <= 0.05 && g <= p[1],
                        "at {d} m the grass is {} m under the road",
                        p[1] - g
                    );
                }
                d += 3.0;
            }
        }
    }

    #[test]
    fn should_keep_the_ground_under_both_legs_of_a_steep_hairpin() {
        // Round a tight bend on a climb, one leg of the road passes within a
        // grid cell of the other, metres higher. Ground held at the height
        // of whichever leg is nearest lifts a triangle over the lower one —
        // the rider was seen riding through grass.
        for (radius, degrees, grade) in [
            (10.0, 75.0, 0.12),
            (12.0, 75.0, -0.15),
            (25.0, 60.0, 0.1),
            (8.0, 75.0, 0.08),
        ] {
            let road = climbing_hairpin(radius, degrees, grade);
            let ground = ground_mesh(&road, GROUND_SPACING_M, 120.0, GROUND_DROP_M, None);
            if let Some(e) = ground_over_road(&road, &ground) {
                panic!("a {radius} m hairpin at {grade}: {e}");
            }
        }
    }

    #[test]
    fn should_keep_the_ground_under_the_foot_of_a_steep_drop() {
        // Down at 12 % and then level, and the other way round: a flat
        // triangle bridging the knee from both sides of it stood half a metre
        // over the road at the foot of Monaco's Basse Corniche.
        for (before, after) in [(-0.12f32, 0.0f32), (0.0, 0.12), (-0.15, 0.05), (0.1, -0.1)] {
            let road: Vec<RoadPoint> = (0..=400)
                .map(|i| {
                    let d = i as f32;
                    // The knee off the grid's lines, as a real one is.
                    let y = if d < 207.0 {
                        before * (d - 207.0)
                    } else {
                        after * (d - 207.0)
                    };
                    RoadPoint {
                        pos: [7.0, y, -d],
                        distance_m: d,
                    }
                })
                .collect();
            let ground = ground_mesh(&road, GROUND_SPACING_M, 120.0, GROUND_DROP_M, None);
            if let Some(e) = ground_over_road(&road, &ground) {
                panic!("{before} then {after}: {e}");
            }
        }
    }

    #[test]
    fn should_leave_the_ground_beside_a_straight_climb_at_the_roads_height() {
        // The hairpin's fix must not dig a trench beside every climb.
        let road = road_of(&straight_route(200, 10.0, 1.0));
        let ground = ground_mesh(&road, GROUND_SPACING_M, 100.0, GROUND_DROP_M, None);
        assert!(ground_over_road(&road, &ground).is_none());
        for v in &ground.vertices {
            let d = -v.pos[2];
            if v.pos[0].abs() < 30.0 && (50.0..1_900.0).contains(&d) {
                let road_y = position_at(&road, d).expect("a road")[1];
                assert!(
                    (v.pos[1] - (road_y - GROUND_DROP_M)).abs() < 0.6,
                    "ground at {:?} beside a road at {road_y}",
                    v.pos
                );
            }
        }
    }

    #[test]
    fn should_keep_the_ground_below_the_road_everywhere_along_it() {
        // Climbing 0.4 m every 10 m. Every ground vertex sits under the
        // nearest road point, so none can rise through the road surface.
        let road = road_of(&straight_route(200, 10.0, 0.4));
        let ground = ground_mesh(&road, GROUND_SPACING_M, 100.0, GROUND_DROP_M, None);
        assert!(!ground.vertices.is_empty());
        // Heights come from the road resampled every few metres, so a vertex
        // sits under the road where it is, not under the nearest GPX point.
        let road = resample(&road, RESAMPLE_M);
        let end = road.last().map_or(0.0, |p| p.distance_m);
        for v in lattice(&ground, GROUND_SPACING_M) {
            // At and past either end the ground falls on below the last
            // point; see
            // `should_keep_the_ground_under_both_legs_of_a_steep_hairpin`.
            if v.pos[2] >= 0.0 || -v.pos[2] >= end {
                continue;
            }
            let d2 = |p: &RoadPoint| (p.pos[0] - v.pos[0]).powi(2) + (p.pos[2] - v.pos[2]).powi(2);
            let best = road.iter().map(d2).fold(f32::MAX, f32::min);
            // A vertex exactly between two road points may take either.
            let under_a_nearest = road
                .iter()
                .filter(|p| d2(p) <= best + 1e-2)
                .any(|p| (v.pos[1] - (p.pos[1] - GROUND_DROP_M)).abs() < 1e-3);
            assert!(
                under_a_nearest,
                "ground at {:?} is not {GROUND_DROP_M} m under its road point",
                v.pos
            );
        }
    }

    #[test]
    fn should_not_reach_further_from_the_road_than_its_radius() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let ground = ground_mesh(&road, 10.0, 100.0, 0.0, None);
        let x0 = road[0].pos[0];
        for v in &ground.vertices {
            assert!((v.pos[0] - x0).abs() <= 100.0 + 1e-3, "{:?}", v.pos);
        }
    }

    #[test]
    fn should_build_the_same_ground_twice() {
        let road = road_of(&straight_route(80, 10.0, 0.2));
        let a = ground_mesh(&road, GROUND_SPACING_M, 200.0, GROUND_DROP_M, None);
        let b = ground_mesh(&road, GROUND_SPACING_M, 200.0, GROUND_DROP_M, None);
        assert_eq!(a, b);
    }

    #[test]
    fn should_build_no_ground_from_nonsense_settings() {
        let road = road_of(&straight_route(10, 10.0, 0.0));
        assert!(ground_mesh(&road, 0.0, 100.0, 0.0, None)
            .vertices
            .is_empty());
        assert!(ground_mesh(&road, 10.0, -1.0, 0.0, None)
            .vertices
            .is_empty());
        assert!(ground_mesh(&[], 10.0, 100.0, 0.0, None).vertices.is_empty());
    }

    #[test]
    fn should_shift_appended_indices_past_existing_vertices() {
        let v = Vertex {
            pos: [0.0; 3],
            colour: [0.0; 4],
        };
        let mut a = Mesh {
            vertices: vec![v; 4],
            indices: vec![0, 1, 2],
        };
        let b = Mesh {
            vertices: vec![v; 3],
            indices: vec![0, 1, 2],
        };
        a.extend(b);
        assert_eq!(a.vertices.len(), 7);
        assert_eq!(a.indices, vec![0, 1, 2, 4, 5, 6]);
    }

    // ── Matrices ──────────────────────────────────────────────────────────────

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn should_put_the_eye_at_the_origin_of_view_space() {
        let v = look_at([12.0, 5.0, -40.0], [12.0, 4.0, -70.0]);
        let p = transform(&v, [12.0, 5.0, -40.0]);
        assert!(
            close(p[0], 0.0) && close(p[1], 0.0) && close(p[2], 0.0),
            "{p:?}"
        );
    }

    #[test]
    fn should_put_the_target_straight_ahead_down_minus_z() {
        let v = look_at([3.0, 1.6, 7.0], [33.0, 1.6, 7.0]);
        let p = transform(&v, [33.0, 1.6, 7.0]);
        assert!(close(p[0], 0.0) && close(p[1], 0.0), "{p:?}");
        assert!(close(p[2], -30.0), "{p:?}");
    }

    #[test]
    fn should_keep_up_up_when_looking_east() {
        let v = look_at([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]);
        let p = transform(&v, [0.0, 1.0, 0.0]);
        assert!(close(p[1], 1.0), "{p:?}");
        // South (+z) is on the right when facing east.
        let right = transform(&v, [0.0, 0.0, 1.0]);
        assert!(close(right[0], 1.0), "{right:?}");
    }

    #[test]
    fn should_not_produce_nan_when_looking_straight_down() {
        let v = look_at([0.0, 10.0, 0.0], [0.0, 0.0, 0.0]);
        assert!(v.iter().all(|x| x.is_finite()), "{v:?}");
    }

    #[test]
    fn should_map_the_near_and_far_planes_to_the_ends_of_depth() {
        let p = perspective(60.0, 16.0 / 9.0, 0.5, 2000.0);
        let near = transform(&p, [0.0, 0.0, -0.5]);
        let far = transform(&p, [0.0, 0.0, -2000.0]);
        assert!(close(near[2] / near[3], -1.0), "{near:?}");
        assert!(close(far[2] / far[3], 1.0), "{far:?}");
    }

    #[test]
    fn should_survive_an_unallocated_widget() {
        let p = perspective(60.0, 0.0, 0.5, 2000.0);
        assert!(p.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn should_multiply_by_identity_without_change() {
        let id = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let v = look_at([1.0, 2.0, 3.0], [4.0, 1.0, -9.0]);
        assert_eq!(mul4(&id, &v), v);
        assert_eq!(mul4(&v, &id), v);
    }

    // ── Terrain ───────────────────────────────────────────────────────────────

    /// Distance from `(x, z)` to the nearest point of `line`, by brute force.
    fn brute_nearest(line: &[RoadPoint], x: f32, z: f32) -> f32 {
        line.iter()
            .map(|p| ((p.pos[0] - x).powi(2) + (p.pos[2] - z).powi(2)).sqrt())
            .fold(f32::MAX, f32::min)
    }

    #[test]
    fn should_find_the_same_nearest_point_as_a_brute_force_search() {
        // A road that wanders, so the nearest point is often not the obvious
        // one and the ring search has to look past its first find.
        let line: Vec<RoadPoint> = (0..400)
            .map(|i| {
                let t = i as f32 * 0.05;
                RoadPoint {
                    pos: [
                        t.sin() * 300.0 + i as f32,
                        0.0,
                        t.cos() * 250.0 - i as f32 * 3.0,
                    ],
                    distance_m: i as f32,
                }
            })
            .collect();
        let index = NearestIndex::new(&line, 1_200.0);
        for gx in -15..15 {
            for gz in -15..15 {
                let (x, z) = (gx as f32 * 97.0, gz as f32 * 113.0 - 600.0);
                let want = brute_nearest(&line, x, z);
                match index.nearest(x, z) {
                    Some((d2, _)) => assert!((d2.sqrt() - want).abs() < 1e-2, "{x},{z}"),
                    None => assert!(want > 1_200.0, "{x},{z} is {want} m from the road"),
                }
            }
        }
    }

    #[test]
    fn should_resample_without_a_gap_wider_than_the_step() {
        let road = road_of(&straight_route(5, 37.0, 1.0));
        let line = resample(&road, RESAMPLE_M);
        assert_eq!(line.first(), road.first());
        assert_eq!(line.last(), road.last());
        for pair in line.windows(2) {
            let gap = ((pair[1].pos[0] - pair[0].pos[0]).powi(2)
                + (pair[1].pos[2] - pair[0].pos[2]).powi(2))
            .sqrt();
            assert!(gap <= RESAMPLE_M + 1e-3, "{gap}");
            assert!(pair[1].distance_m >= pair[0].distance_m);
        }
    }

    /// The ground grid's own vertices, those on its lattice of `spacing`,
    /// without the verge laid over it.
    fn lattice(ground: &Mesh, spacing: f32) -> impl Iterator<Item = &Vertex> {
        let on = move |v: f32| ((v / spacing).round() * spacing - v).abs() < 1e-3;
        ground
            .vertices
            .iter()
            .filter(move |v| on(v.pos[0]) && on(v.pos[2]))
    }

    /// A hillside rising 0.5 m for every metre east and west of the road,
    /// standing `above` metres over it — far steeper than any road's verge.
    fn valley(above: f32) -> impl Fn(f32, f32) -> Option<f32> {
        move |x: f32, _z: f32| Some(10.0 + above + 0.5 * x.abs())
    }

    #[test]
    fn should_hold_the_shoulder_flat_under_the_road_however_steep_the_land() {
        let road = road_of(&straight_route(200, 10.0, 0.0));
        let line = resample(&road, RESAMPLE_M);
        let land = valley(40.0);
        let ground = ground_mesh(&road, GROUND_SPACING_M, 400.0, GROUND_DROP_M, Some(&land));
        let mut held = 0;
        for v in lattice(&ground, GROUND_SPACING_M) {
            if brute_nearest(&line, v.pos[0], v.pos[2]) <= SHOULDER_M {
                held += 1;
                assert!(
                    (v.pos[1] - (10.0 - GROUND_DROP_M)).abs() < 1e-3,
                    "shoulder vertex at {:?} rose with the hillside",
                    v.pos
                );
            }
        }
        assert!(held > 0);
    }

    #[test]
    fn should_reach_the_lands_own_shape_far_from_the_road() {
        // The land here is 40 m higher than the GPX says the road is. Near the
        // road that is taken up as an offset; far away the land keeps its
        // shape, shifted by the route's typical difference.
        let road = road_of(&straight_route(200, 10.0, 0.0));
        let line = resample(&road, RESAMPLE_M);
        let land = valley(40.0);
        let ground = ground_mesh(&road, GROUND_SPACING_M, 1_000.0, 0.0, Some(&land));
        // At the road the land reads 50 m and the road is at 10: −40.
        let typical = -40.0;
        let far: Vec<&Vertex> = ground
            .vertices
            .iter()
            .filter(|v| brute_nearest(&line, v.pos[0], v.pos[2]) > LOCAL_FIT_END_M + 1.0)
            .collect();
        assert!(!far.is_empty());
        for v in far {
            let raw = 50.0 + 0.5 * v.pos[0].abs();
            assert!((v.pos[1] - (raw + typical)).abs() < 1e-2, "{:?}", v.pos);
        }
    }

    #[test]
    fn should_rise_steadily_from_the_shoulder_to_the_hills() {
        let road = road_of(&straight_route(200, 10.0, 0.0));
        let land = valley(0.0);
        let ground = ground_mesh(&road, 10.0, 400.0, 0.0, Some(&land));
        // Walk east along one row: never a step down on a hillside going up.
        let z = (road[100].pos[2] / 10.0).round() * 10.0;
        let mut row: Vec<[f32; 3]> = lattice(&ground, 10.0)
            .filter(|v| (v.pos[2] - z).abs() < 0.5 && v.pos[0] >= 0.0)
            .map(|v| v.pos)
            .collect();
        row.sort_by(|a, b| a[0].total_cmp(&b[0]));
        assert!(row.len() > 10);
        for w in row.windows(2) {
            assert!(w[1][1] >= w[0][1] - 1e-3, "{:?} then {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn should_fall_back_to_flat_ground_where_the_land_is_unknown() {
        let road = road_of(&straight_route(100, 10.0, 0.0));
        let patchy = |x: f32, _z: f32| (x < 0.0).then_some(60.0);
        let ground = ground_mesh(&road, GROUND_SPACING_M, 400.0, GROUND_DROP_M, Some(&patchy));
        for v in lattice(&ground, GROUND_SPACING_M) {
            assert!(v.pos[1].is_finite());
            if v.pos[0] > 0.0 {
                assert!(
                    (v.pos[1] - (10.0 - GROUND_DROP_M)).abs() < 1e-3,
                    "{:?}",
                    v.pos
                );
            }
        }
    }

    #[test]
    fn should_shape_nothing_when_given_no_land_at_all() {
        let road = road_of(&straight_route(100, 10.0, 0.2));
        let none = |_: f32, _: f32| None;
        assert_eq!(
            ground_mesh(&road, GROUND_SPACING_M, 300.0, GROUND_DROP_M, Some(&none)),
            ground_mesh(&road, GROUND_SPACING_M, 300.0, GROUND_DROP_M, None),
        );
    }

    // ── The route over itself ─────────────────────────────────────────────────

    #[test]
    fn should_never_draw_water_above_the_road_beside_it() {
        // Rule 7: water within 30 m of the road's edge must lie at least half
        // a metre below it — streams, ditches, pools, an oasis and the sea,
        // in every scenery, on a road that rises and falls.
        let mut route = straight_route(400, 10.0, 0.0);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.elevation_m = 10.0 + (i as f32 * 0.05).sin() * 6.0;
        }
        for scenery in Scenery::CHOICES {
            let scene = CourseScene::build(&route, scenery).expect("a scene");
            let line = resample(&scene.road, RESAMPLE_M);
            let index = NearestIndex::new(&line, DEFAULT_EXTENT_M + 40.0);
            let mut water_seen = 0;
            for v in &scene.mesh.vertices {
                if v.colour[3] != MATERIAL_WATER {
                    continue;
                }
                water_seen += 1;
                let Some((d2, i)) = index.nearest(v.pos[0], v.pos[2]) else {
                    continue;
                };
                if d2.sqrt() <= DEFAULT_EXTENT_M + 30.0 {
                    assert!(
                        v.pos[1] <= line[i].pos[1] - 0.5,
                        "{scenery:?}: water at {:?}, {:.1} m from a road at {} m",
                        v.pos,
                        d2.sqrt(),
                        line[i].pos[1]
                    );
                }
            }
            let wet = matches!(
                scenery,
                Scenery::Countryside
                    | Scenery::Only(
                        biome::Biome::Polder | biome::Biome::Coast | biome::Biome::Wetland
                    )
            );
            if wet {
                assert!(water_seen > 0, "{scenery:?} has no water in 4 km");
            }
        }
    }

    #[test]
    fn should_keep_every_prop_off_the_track_and_every_animal_well_clear() {
        // Rule 5 and the skill's encounters rule: nothing stands on the
        // asphalt — not a house, not a lamp post — and nothing that moves
        // comes within the animals' clearance of the edge.
        let route = straight_route(400, 10.0, 0.0);
        for scenery in Scenery::CHOICES {
            let scene = CourseScene::build(&route, scenery).expect("a scene");
            let line = resample(&scene.road, RESAMPLE_M);
            let index = NearestIndex::new(&line, 200.0);
            for p in &scene.props {
                let Some((d2, _)) = index.nearest(p.pos[0], p.pos[2]) else {
                    continue;
                };
                let from_edge = d2.sqrt() - DEFAULT_EXTENT_M;
                assert!(
                    from_edge >= 3.0,
                    "{scenery:?}: {p:?} is {from_edge} m from the edge"
                );
                let moves = matches!(
                    p.motion,
                    crate::data::props::Motion::Graze | crate::data::props::Motion::Bob
                );
                if moves {
                    assert!(
                        from_edge >= animals::CLEAR_OF_ROAD_M - 0.01,
                        "{scenery:?}: {p:?} grazes {from_edge} m from the edge"
                    );
                }
            }
        }
    }

    #[test]
    fn should_keep_trees_and_herds_off_the_ground_a_house_takes() {
        let route = straight_route(400, 10.0, 0.0);
        let scene =
            CourseScene::build(&route, Scenery::Only(biome::Biome::Urban)).expect("a scene");
        let houses: Vec<&Prop> = scene
            .props
            .iter()
            .filter(|p| p.shape == crate::data::props::Shape::House)
            .collect();
        assert!(houses.len() > 100, "{} houses in a 4 km town", houses.len());
        for t in scene.props.iter().filter(|p| p.shape.is_tree()) {
            for h in &houses {
                let d = (t.pos[0] - h.pos[0]).hypot(t.pos[2] - h.pos[2]);
                assert!(
                    d >= 3.0,
                    "a tree at {:?} stands in a house at {:?}",
                    t.pos,
                    h.pos
                );
            }
        }
        // Houses never stand in one another either.
        for (k, a) in houses.iter().enumerate() {
            for b in &houses[k + 1..] {
                let d = (a.pos[0] - b.pos[0]).hypot(a.pos[2] - b.pos[2]);
                assert!(d >= 4.0, "two houses {d} m apart at {:?}", a.pos);
            }
        }
    }

    // ── Scenery ───────────────────────────────────────────────────────────────

    #[test]
    fn should_lay_the_same_road_whatever_the_scenery() {
        // Rules 1–4: the scenery is off the road. A road that rises and
        // falls, built in every choice, is the same road every time.
        let mut route = straight_route(150, 10.0, 0.0);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.elevation_m = 10.0 + (i as f32 * 0.07).sin() * 8.0;
        }
        let base = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        for s in Scenery::CHOICES {
            let scene = CourseScene::build(&route, s).expect("a scene");
            assert!(scene.road == base.road, "{s:?} moved the road");
        }
    }

    #[test]
    fn should_paint_one_chosen_biome_over_the_whole_ride() {
        let route = straight_route(150, 10.0, 0.0);
        let desert =
            CourseScene::build(&route, Scenery::Only(biome::Biome::Desert)).expect("a scene");
        let want = biome::Biome::Desert.def().palette.sky_horizon;
        for d in [0.0, 700.0, 1_490.0] {
            assert_eq!(desert.sky_at(d).horizon, want, "at {d} m");
        }
        let countryside = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        assert_ne!(countryside.sky_at(700.0).horizon, want);
        assert!(
            desert.mesh != countryside.mesh,
            "the ground was not repainted"
        );
    }

    #[test]
    fn should_stand_one_biomes_horizon_round_the_rider_and_only_that_one() {
        let route = straight_route(150, 10.0, 0.0);
        for (scenery, want) in [
            (Scenery::Countryside, biome::Biome::Temperate),
            (Scenery::Only(biome::Biome::Desert), biome::Biome::Desert),
            (Scenery::Only(biome::Biome::Polder), biome::Biome::Polder),
        ] {
            let scene = CourseScene::build(&route, scenery).expect("a scene");
            let kinds: Vec<biome::Biome> = scene.horizons.iter().map(|(b, _)| *b).collect();
            assert_eq!(kinds, vec![want], "{scenery:?}");
            for d in [0.0, 700.0, 1_490.0] {
                let mut full = [0.0; biome::COUNT];
                full[want as usize] = 1.0;
                assert_eq!(scene.sky_at(d).backdrop, full, "{scenery:?} at {d}");
            }
        }
    }

    #[test]
    fn should_sink_one_horizon_as_the_next_rises_on_a_mixed_ride() {
        let route = straight_route(4_001, 10.0, 0.0);
        let scene = CourseScene::build(&route, Scenery::Mixed).expect("a scene");
        let world = World::new(&scene.road);
        let length = scene.road.last().expect("a road").distance_m;
        let segs = biome::mix_segments(world.seed(), length);
        assert!(segs.len() >= 3);
        // Every biome of the mix has its horizon, and no other does.
        let mut want: Vec<biome::Biome> = segs.iter().map(|s| s.1).collect();
        want.sort_by_key(|b| *b as usize);
        want.dedup();
        let have: Vec<biome::Biome> = scene.horizons.iter().map(|(b, _)| *b).collect();
        assert_eq!(have, want);
        let (border, before, after) = (segs[1].0, segs[0].1, segs[1].1);
        let at = |d: f32| scene.sky_at(d).backdrop;
        assert_eq!(at(border - 400.0)[before as usize], 1.0);
        assert!((at(border)[before as usize] - 0.5).abs() < 0.05);
        assert!((at(border)[after as usize] - 0.5).abs() < 0.05);
        assert_eq!(at(border + 400.0)[after as usize], 1.0);
    }

    #[test]
    fn should_change_biome_along_a_mixed_ride_where_the_mix_says() {
        let route = straight_route(4_001, 10.0, 0.0);
        let road = track::lay(&route);
        let world = World::new(&road);
        let look = look_for(&road, 52.0, &world, Scenery::Mixed);
        let length = road.last().expect("a road").distance_m;
        let segs = biome::mix_segments(world.seed(), length);
        assert!(segs.len() >= 3, "40 km mixes at least three stretches");
        for (k, &(start, b)) in segs.iter().enumerate() {
            let end = segs.get(k + 1).map_or(length, |s| s.0);
            let p = position_at(&road, (start + end) / 2.0).expect("a point");
            // On the road and a kilometre off it, the stretch's biome.
            assert_eq!(
                look.biomes.dominant_at(p[0], p[2]),
                b,
                "stretch {k} on the road"
            );
            assert_eq!(
                look.biomes.dominant_at(p[0] + 1_000.0, p[2]),
                b,
                "stretch {k} off it"
            );
        }
        // And the same mix every time.
        assert!(look_for(&road, 52.0, &World::new(&road), Scenery::Mixed) == look);
    }

    // ── Determinism ───────────────────────────────────────────────────────────

    #[test]
    fn should_build_the_same_world_every_time() {
        // Everything a build picks — hills, fields, woods, streams, trees,
        // flocks, mountains, clouds. Each build's hash maps are seeded
        // afresh, so an order taken from one shows here.
        let route = straight_route(300, 10.0, 0.05);
        let first = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        assert!(first.mesh.vertices.len() > 1_000, "a world was built");
        assert!(first.props.len() > 100, "{} props", first.props.len());
        for _ in 0..2 {
            assert!(
                CourseScene::build(&route, Scenery::Countryside).expect("a scene") == first,
                "two builds differ"
            );
        }
        // And with land of the caller's own shape.
        let hills =
            |x: f32, z: f32| Some(10.0 + (x * 0.01).sin() * 20.0 + (z * 0.013).cos() * 15.0);
        let a =
            CourseScene::build_from(&route, Some(&hills), Scenery::Countryside).expect("a scene");
        let b =
            CourseScene::build_from(&route, Some(&hills), Scenery::Countryside).expect("a scene");
        assert!(a == b);
    }

    #[test]
    fn should_keep_every_animal_off_the_road_in_a_whole_world() {
        // Rule 5 on a real build: flocks graze and birds circle well clear.
        let route = straight_route(600, 10.0, 0.0);
        let scene = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        let line = resample(&scene.road, RESAMPLE_M);
        let animals: Vec<&Prop> = scene
            .props
            .iter()
            .filter(|p| {
                matches!(
                    p.shape,
                    crate::data::props::Shape::Sheep | crate::data::props::Shape::Bird
                )
            })
            .collect();
        assert!(animals.len() > 30, "{} animals", animals.len());
        for a in animals {
            let reach = if a.motion == crate::data::props::Motion::Orbit {
                crate::data::props::ORBIT_M
            } else {
                0.0
            };
            let d = brute_nearest(&line, a.pos[0], a.pos[2]) - DEFAULT_EXTENT_M - reach;
            assert!(
                d >= animals::CLEAR_OF_ROAD_M - 0.01,
                "{a:?} {d} m from the road"
            );
        }
    }

    #[test]
    fn should_keep_every_tree_off_the_road_in_a_whole_world() {
        let route = straight_route(400, 10.0, 0.0);
        let scene = CourseScene::build(&route, Scenery::Countryside).expect("a scene");
        let line = resample(&scene.road, RESAMPLE_M);
        for t in scene.props.iter().filter(|p| p.shape.is_tree()) {
            let d = brute_nearest(&line, t.pos[0], t.pos[2]) - DEFAULT_EXTENT_M;
            assert!(
                d >= trees::CLEAR_OF_ROAD_M - 0.01,
                "{t:?} {d} m from the road"
            );
        }
    }
}

#[cfg(test)]
mod load {
    use super::*;
    use crate::data::props::{self, Shape};

    /// What the view draws of the props at the busiest point of a route, in
    /// every scenery: triangles and draw calls.
    ///
    /// ```text
    /// CYCLE_3D_GPX=route.gpx cargo test --release prop_load -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn prop_load() {
        let gpx = std::env::var("CYCLE_3D_GPX").expect("CYCLE_3D_GPX");
        let route = Route::from_gpx_path(std::path::Path::new(&gpx)).expect("a GPX");
        let reach: f32 = std::env::var("CYCLE_3D_REACH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_300.0);
        let tris: std::collections::HashMap<Shape, usize> = Shape::ALL
            .iter()
            .map(|&s| (s, s.mesh().indices.len() / 3))
            .collect();
        for scenery in Scenery::CHOICES
            .iter()
            .copied()
            .chain([Scenery::Countryside])
        {
            let scene = CourseScene::build(&route, scenery).expect("a scene");
            let batches = props::batches(&scene.props);
            let length = scene.road.last().map_or(0.0, |p| p.distance_m);
            let (mut worst_tris, mut worst_draws, mut worst_at) = (0usize, 0usize, 0.0f32);
            let mut d = 0.0;
            while d < length {
                let eye = position_at(&scene.road, d).expect("a point");
                let ahead = position_at(&scene.road, d + 10.0).expect("a point");
                let (fx, fz) = (ahead[0] - eye[0], ahead[2] - eye[2]);
                let len = fx.hypot(fz).max(1e-3);
                let forward = (fx / len, fz / len);
                let (mut t, mut n) = (0usize, 0usize);
                for b in &batches {
                    for c in &b.chunks {
                        // The view's 16:9 at 60° tall, a little wider.
                        if props::in_view(c, eye, forward, 1.18, reach) {
                            t += c.count as usize * tris[&b.shape];
                            n += 1;
                        }
                    }
                }
                if t > worst_tris {
                    (worst_tris, worst_draws, worst_at) = (t, n, d);
                }
                d += 250.0;
            }
            eprintln!(
                "{:>14}: {:>7} props, world {:>7} tris; worst {:>9} prop tris, {:>4} draws at {:.0} m",
                scenery.key(),
                scene.props.len(),
                scene.mesh.indices.len() / 3,
                worst_tris,
                worst_draws,
                worst_at
            );
        }
    }
}
