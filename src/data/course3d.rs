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
//! The road follows the GPX's own elevation, not any terrain model, because the
//! GPX is what the trainer is driven from: the road the rider sees climbing is
//! the climb they feel. The ground is fitted to the road, never the other way
//! round.

use crate::data::osm::AreaKind;
use crate::data::roadway;
use crate::data::route::Route;
use crate::data::scenery::{self, Scenery, WATER_RGB};

/// Height of the rider's eye above the road surface, in metres. A rider on
/// the hoods of a road bike, not someone standing at the roadside — the rider
/// found 1.6 m sat noticeably too high.
pub const EYE_HEIGHT_M: f32 = 1.3;

/// How far up the road the camera aims, in metres. Far enough that a GPS
/// wobble in the next point or two does not swing the view, near enough that
/// the camera still turns into a bend rather than across it.
pub const LOOK_AHEAD_M: f32 = 30.0;

/// GPX points either side averaged into each centreline point. A recorded ride
/// is about one point every 8–9 m with a metre or two of GPS scatter, which
/// drawn raw reads as a road with a kink every few metres.
const SMOOTH_POINTS: usize = 3;

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

/// How far the road the view draws where the map says nothing reaches from
/// the rider's line — `roadway::Profile::DEFAULT.extent()`.
pub const DEFAULT_EXTENT_M: f32 = 4.5;

/// The shoulder beside that road.
#[cfg(test)]
pub const SHOULDER_M: f32 = SHOULDER_BASE_M + DEFAULT_EXTENT_M;

/// Buildings keep this much clear of the road's edge: a pavement. The GPX can
/// sit a few metres off the street the map draws, and a building on the tarmac
/// is worse than a missing one.
pub const PAVEMENT_M: f32 = 1.5;

/// Spacing the centreline is resampled at before distances to it are taken,
/// in metres. A recorded GPX is a point every 8–9 m; a planned one can be a
/// point every 100.
const RESAMPLE_M: f32 = 5.0;

const _: () =
    assert!(SHOULDER_BASE_M >= GROUND_SPACING_M * std::f32::consts::SQRT_2 + RESAMPLE_M / 2.0);

/// Under the ridden road's bridges the ground drops this far below the deck,
/// this far either side of the road's edge — room for what the bridge spans.
const UNDER_BRIDGE_M: f32 = 6.0;
const UNDER_BRIDGE_REACH_M: f32 = 8.0;

/// Beyond the shoulder the terrain blends in, reaching its own height here.
const BLEND_END_M: f32 = 120.0;

/// The elevation tiles and the GPX rarely agree on height to the metre — a
/// barometer drifts, a tile averages a valley. Near the road the land is
/// shifted by the local difference so the road sits on it; by this distance
/// the shift has eased to the route's typical difference, so two stretches of
/// road that disagree do not tear the land between them.
const LOCAL_FIT_END_M: f32 = 600.0;

/// Raw elevation below this is seabed: the tiles carry ocean depths, which
/// would draw the Marmara coast as a trench. Seabed is flattened to sea level
/// and coloured as water. Low enough that a Dutch polder stays land.
pub const SEABED_BELOW_M: f32 = -2.0;

/// What a vertex is made of. Sent to the shader in the colour's alpha, so one
/// vertex layout carries everything.
pub const MATERIAL_PLAIN: f32 = 0.0;
pub const MATERIAL_GROUND: f32 = 1.0;
/// Road surface: a fine asphalt grain.
pub const MATERIAL_ROAD: f32 = 2.0;
/// A building's walls: storeys and windows, laid out by the vertex's wall
/// coordinates (see [`Vertex::uv`]). The fraction carries a seed for which
/// windows are lit — see [`wall_material`].
pub const MATERIAL_WALL: f32 = 3.0;

/// Wall coordinates for anything that is not a wall.
pub const NO_UV: [f32; 2] = [0.0, 0.0];

/// The wall material for a building whose window pattern is seeded by `seed`
/// (any value; only its fraction is used).
pub fn wall_material(seed: f32) -> f32 {
    // Kept below 1, or the fraction would roll over into the next material.
    MATERIAL_WALL + seed.rem_euclid(1.0).min(0.999)
}

/// Scene colours, in linear RGB. Fixed rather than theme-derived, the same as
/// the OSM tiles under the 2D map: a road is grey at night in dark mode too.
const GROUND_RGB: [f32; 3] = [0.36, 0.52, 0.27];

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
/// Equirectangular: exact enough over a ride's extent (the error at 50 km is
/// centimetres), and cheap enough to run per vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalFrame {
    lat0: f64,
    lng0: f64,
    m_per_deg_lat: f64,
    m_per_deg_lng: f64,
}

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
        // Mean metres per degree on the WGS-84 ellipsoid — the first terms of
        // the standard series, good to well under a metre per degree.
        let phi = lat.to_radians();
        let m_per_deg_lat = 111_132.92 - 559.82 * (2.0 * phi).cos() + 1.175 * (4.0 * phi).cos();
        let m_per_deg_lng = 111_412.84 * phi.cos() - 93.5 * (3.0 * phi).cos();
        Self {
            lat0: lat,
            lng0: lng,
            m_per_deg_lat,
            m_per_deg_lng,
        }
    }

    /// The frame's origin, `(lat, lng)`.
    pub fn origin(self) -> (f64, f64) {
        (self.lat0, self.lng0)
    }

    /// `(east, north)` in metres from the origin.
    pub fn to_local(self, lat: f64, lng: f64) -> (f64, f64) {
        (
            (lng - self.lng0) * self.m_per_deg_lng,
            (lat - self.lat0) * self.m_per_deg_lat,
        )
    }

    /// `(lat, lng)` of a point `east`, `north` metres from the origin.
    pub fn to_geo(self, east: f64, north: f64) -> (f64, f64) {
        (
            self.lat0 + north / self.m_per_deg_lat,
            self.lng0 + east / self.m_per_deg_lng,
        )
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
    /// On a wall: window bays along it (whole numbers at the bay edges, or
    /// negative for a wall too short for any) and metres up it from the
    /// building's ground. Elsewhere [`NO_UV`].
    pub uv: [f32; 2],
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

/// Everything the view needs for one route.
#[derive(Debug, Clone, PartialEq)]
pub struct CourseScene {
    pub road: Vec<RoadPoint>,
    pub mesh: Mesh,
}

impl CourseScene {
    /// Build the scene for `route`: the road, its paint, and flat ground.
    /// `None` for a route too short to draw — fewer than two points.
    #[cfg(test)]
    pub fn build(route: &Route) -> Option<Self> {
        Self::build_with(route, None, None)
    }

    /// Build the scene with the land shaped by `dem` where it has heights.
    ///
    /// With `features`, the ground is coloured by land use, the sea is laid
    /// flat and blue, and the buildings stand along the road.
    pub fn build_with(
        route: &Route,
        dem: Option<&crate::data::terrain::Dem>,
        features: Option<&crate::data::osm::Features>,
    ) -> Option<Self> {
        let frame = LocalFrame::around(route)?;
        let road = centreline(route, &frame);
        if road.len() < 2 {
            return None;
        }
        let sample = |x: f32, z: f32| {
            let (lat, lng) = frame.to_geo(f64::from(x), -f64::from(z));
            dem?.height_at(lat, lng)
        };
        let terrain: Option<Terrain> = dem.map(|_| &sample as Terrain);
        let scenery = features.map(|f| Scenery::new(f, &frame));
        // The road the rider is on, matched from the map where it has one.
        let ridden =
            roadway::Ridden::new(&road, scenery.as_ref().map_or(&[][..], |s| &s.roads[..]));
        let mut mesh = world_mesh(&road, &ridden, Grid::VIEW, terrain, scenery.as_ref());
        mesh.extend(roadway::ridden_mesh(&road, &ridden));
        Some(Self { road, mesh })
    }
}

/// The route's centreline in scene space, smoothed in plan and in height.
///
/// Points are averaged over a window of [`SMOOTH_POINTS`] either side, which
/// narrows towards the ends so the first and last points stay where they are.
pub fn centreline(route: &Route, frame: &LocalFrame) -> Vec<RoadPoint> {
    let raw: Vec<[f64; 3]> = route
        .points
        .iter()
        .map(|p| {
            let (e, n) = frame.to_local(p.lat, p.lng);
            [e, p.elevation_m as f64, -n]
        })
        .collect();
    let len = raw.len();
    (0..len)
        .map(|i| {
            let k = SMOOTH_POINTS.min(i).min(len - 1 - i);
            let window = &raw[i - k..=i + k];
            let n = window.len() as f64;
            let sum = window.iter().fold([0.0; 3], |acc, p| {
                [acc[0] + p[0], acc[1] + p[1], acc[2] + p[2]]
            });
            RoadPoint {
                pos: [
                    (sum[0] / n) as f32,
                    (sum[1] / n) as f32,
                    (sum[2] / n) as f32,
                ],
                distance_m: route.points[i].distance_m,
            }
        })
        .collect()
}

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

/// The rider's eye at `distance_m`, looking [`LOOK_AHEAD_M`] up the road.
///
/// Near the finish the point ahead runs off the end of the road, so the last
/// stretch's direction is carried on past it rather than letting the target
/// collapse onto the eye — a camera looking at its own position has no
/// direction at all.
pub fn camera_pose(road: &[RoadPoint], distance_m: f32) -> Option<CameraPose> {
    let start = road.first()?.distance_m;
    let end = road.last()?.distance_m;
    let d = distance_m.clamp(start, end);
    let here = position_at(road, d)?;

    let ahead_d = d + LOOK_AHEAD_M;
    let ahead = if ahead_d <= end {
        position_at(road, ahead_d)?
    } else {
        // Carry the final direction on past the finish.
        let tail = position_at(road, (end - LOOK_AHEAD_M).max(start))?;
        let last = position_at(road, end)?;
        let dir = normalise_xz(sub3(last, tail)).unwrap_or([0.0, 0.0, -1.0]);
        let over = ahead_d - end;
        [last[0] + dir[0] * over, last[1], last[2] + dir[2] * over]
    };
    // A road that doubles back on itself within the look-ahead can put the
    // point ahead directly above or below the eye; aim north rather than up.
    let ahead = if normalise_xz(sub3(ahead, here)).is_none() {
        [here[0], here[1], here[2] - LOOK_AHEAD_M]
    } else {
        ahead
    };

    // The GPX is the line the rider rode — the middle of the right-hand lane,
    // which the road is built around (see `roadway`) — so the eye is on it.
    Some(CameraPose {
        eye: [here[0], here[1] + EYE_HEIGHT_M, here[2]],
        // Aimed slightly below eye level, as a rider looks at the road.
        target: [ahead[0], ahead[1] + EYE_HEIGHT_M * 0.6, ahead[2]],
    })
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
    world_mesh(road, &roadway::Ridden::new(road, &[]), grid, terrain, None)
}

/// The ground, as [`ground_mesh`], and with `scenery` everything on it: land
/// use coloured in, the sea laid flat at sea level and coloured as water, and
/// the buildings standing on the very ground drawn — so none floats or sinks.
///
/// `extents[i]` is how far the road reaches from the rider's line at
/// `road[i]` (missing entries are the default road); the flat shoulder and the
/// buildings' clearance follow it. `matched` names the mapped roads that are
/// the ridden road, so they are not drawn a second time as other roads.
pub fn world_mesh(
    road: &[RoadPoint],
    ridden: &roadway::Ridden,
    grid: Grid,
    terrain: Option<Terrain>,
    scenery: Option<&Scenery>,
) -> Mesh {
    let extents = ridden.extents();
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
    // How far the road reaches at each resampled point: the wider of the two
    // measured points either side, so the shoulder never falls short.
    let reach: Vec<f32> = line
        .iter()
        .map(|p| {
            let i = road.partition_point(|q| q.distance_m <= p.distance_m);
            let at = |k: usize| extents.get(k).copied().unwrap_or(DEFAULT_EXTENT_M);
            at(i.saturating_sub(1)).max(at(i.min(road.len().saturating_sub(1))))
        })
        .collect();

    // How far the terrain sits from the road, point by point, and typically.
    let raw_at = |x: f32, z: f32| terrain.and_then(|t| t(x, z)).map(sea_level);
    // Whether the road is a bridge at each resampled point: either measured
    // point around it being one is enough.
    let on_bridge: Vec<bool> = line
        .iter()
        .map(|p| {
            let i = road.partition_point(|q| q.distance_m <= p.distance_m);
            let at = |k: usize| ridden.levels.get(k).is_some_and(|l| l.bridge);
            at(i.saturating_sub(1)) || at(i.min(road.len().saturating_sub(1)))
        })
        .collect();
    // A bridge stands above the land by design; its height says nothing about
    // where the land is, and fitting to it would lift the valley to the deck.
    let offsets: Vec<Option<f32>> = line
        .iter()
        .zip(&on_bridge)
        .map(|(p, &bridge)| {
            (!bridge)
                .then(|| raw_at(p.pos[0], p.pos[2]).map(|h| p.pos[1] - h))
                .flatten()
        })
        .collect();
    let typical = median(offsets.iter().flatten().copied()).unwrap_or(0.0);

    let (mut x_lo, mut x_hi, mut z_lo, mut z_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in road {
        x_lo = x_lo.min(p.pos[0]);
        x_hi = x_hi.max(p.pos[0]);
        z_lo = z_lo.min(p.pos[2]);
        z_hi = z_hi.max(p.pos[2]);
    }
    // Snap the grid to multiples of the spacing so two builds of one route
    // produce the same vertices.
    let i_lo = ((x_lo - radius) / spacing).floor() as i64;
    let i_hi = ((x_hi + radius) / spacing).ceil() as i64;
    let j_lo = ((z_lo - radius) / spacing).floor() as i64;
    let j_hi = ((z_hi + radius) / spacing).ceil() as i64;
    let cols = (i_hi - i_lo + 1) as usize;
    let rows = (j_hi - j_lo + 1) as usize;

    let water = [WATER_RGB[0], WATER_RGB[1], WATER_RGB[2], MATERIAL_PLAIN];
    let land_at = |x: f32, z: f32| match scenery.and_then(|s| s.area_at(x, z)) {
        Some(AreaKind::Water) => water,
        Some(kind) => {
            let rgb = scenery::area_rgb(kind);
            [rgb[0], rgb[1], rgb[2], MATERIAL_GROUND]
        }
        None => [GROUND_RGB[0], GROUND_RGB[1], GROUND_RGB[2], MATERIAL_GROUND],
    };
    // The ground's height and colour at any point within reach: the grid's
    // vertices, and the corners the buildings stand on.
    let sample_one = |x: f32, z: f32| -> Option<(f32, [f32; 4])> {
        let (d2, i) = index.nearest(x, z)?;
        let dist = d2.sqrt();
        let flat = line[i].pos[1] - drop;
        let raw = terrain.and_then(|t| t(x, z));
        if on_bridge[i] && dist <= SHOULDER_BASE_M + reach[i] {
            // Under and beside a bridge the ground is the land's own, never
            // above the deck, and dropped away beneath it so what passes under
            // can be seen.
            let land = raw.map(|raw| {
                let sea = raw < SEABED_BELOW_M || scenery.is_some_and(|s| s.is_sea(x, z, raw));
                let shift = offsets[i].unwrap_or(typical);
                (if sea { 0.0 } else { sea_level(raw) } + shift, sea)
            });
            let mut y = land.map_or(flat, |(h, _)| h.min(flat));
            if dist <= reach[i] + UNDER_BRIDGE_REACH_M {
                y = y.min(line[i].pos[1] - UNDER_BRIDGE_M);
            }
            let colour = match land {
                Some((_, true)) => water,
                _ => land_at(x, z),
            };
            return Some((y, colour));
        }
        Some(match raw {
            Some(raw) if dist > SHOULDER_BASE_M + reach[i] => {
                let sea = raw < SEABED_BELOW_M || scenery.is_some_and(|s| s.is_sea(x, z, raw));
                let near = offsets[i].unwrap_or(typical);
                let shift = lerp(
                    near,
                    typical,
                    smoothstep(BLEND_END_M, LOCAL_FIT_END_M, dist),
                );
                let fitted = if sea { 0.0 } else { sea_level(raw) } + shift;
                let y = lerp(
                    flat,
                    fitted,
                    smoothstep(SHOULDER_BASE_M + reach[i], BLEND_END_M, dist),
                );
                (y, if sea { water } else { land_at(x, z) })
            }
            _ => (flat, land_at(x, z)),
        })
    };

    let widest = reach.iter().copied().fold(DEFAULT_EXTENT_M, f32::max);
    // Where the route passes this way more than once — a loop, an interchange
    // — the ground is never above the flat beside a lower pass, or it would
    // bury that road under the shoulder of the one above.
    let under_lower_pass = |x: f32, z: f32, i: usize, y: f32| -> f32 {
        let mut y = y;
        index.for_each_within(x, z, SHOULDER_BASE_M + widest, |j| {
            let q = &line[j];
            if (q.distance_m - line[i].distance_m).abs() > roadway::SAME_PASS_M
                && (q.pos[0] - x).hypot(q.pos[2] - z) <= SHOULDER_BASE_M + reach[j]
            {
                y = y.min(q.pos[1] - drop);
            }
        });
        y
    };
    let sample = |x: f32, z: f32| -> Option<(f32, [f32; 4])> {
        let (y, colour) = sample_one(x, z)?;
        let (d2, i) = index.nearest(x, z)?;
        if d2.sqrt() > SHOULDER_BASE_M + widest {
            return Some((y, colour));
        }
        Some((under_lower_pass(x, z, i, y), colour))
    };

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
                uv: NO_UV,
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

    // The ridden road's own bridges and tunnels, their pillars on this ground.
    mesh.extend(roadway::ridden_structures(road, ridden, |x, z| {
        sample(x, z).map(|(y, _)| y)
    }));

    if let Some(scenery) = scenery {
        // A building is blocked if any point of the road comes within the
        // clearance of its outline, or lies inside it.
        let blocked = |outline: &[(f32, f32)]| {
            let (cx, cz) = outline
                .iter()
                .fold((0.0, 0.0), |(a, b), &(x, z)| (a + x, b + z));
            let (cx, cz) = (cx / outline.len() as f32, cz / outline.len() as f32);
            let span = outline
                .iter()
                .map(|&(x, z)| ((x - cx).powi(2) + (z - cz).powi(2)).sqrt())
                .fold(0.0f32, f32::max);
            let mut hit = false;
            index.for_each_within(cx, cz, span + widest + PAVEMENT_M, |i| {
                if hit {
                    return;
                }
                let p = line[i].pos;
                hit = scenery::near_or_inside(outline, p[0], p[2], reach[i] + PAVEMENT_M);
            });
            hit
        };
        mesh.extend(scenery::building_mesh(
            &scenery.buildings,
            scenery.region,
            |x, z| sample(x, z).map(|(y, _)| y),
            blocked,
        ));
        mesh.extend(roadway::other_roads_mesh(
            &scenery.roads,
            road,
            ridden,
            |x, z| sample(x, z).map(|(y, _)| y),
        ));
    }
    mesh
}

/// Seabed raised to sea level; land left alone.
fn sea_level(raw: f32) -> f32 {
    if raw < SEABED_BELOW_M {
        0.0
    } else {
        raw
    }
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

    fn road_of(route: &Route) -> Vec<RoadPoint> {
        let frame = LocalFrame::around(route).expect("a route with points");
        centreline(route, &frame)
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
    fn should_put_a_degree_of_latitude_at_about_111_km() {
        let frame = LocalFrame::at(52.0, 6.0);
        let (_, n) = frame.to_local(53.0, 6.0);
        assert!((n - 111_270.0).abs() < 100.0, "got {n}");
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
        assert!(CourseScene::build(&route).is_none());
    }

    #[test]
    fn should_not_build_a_scene_from_a_single_point() {
        assert!(CourseScene::build(&straight_route(1, 10.0, 0.0)).is_none());
    }

    // ── Centreline ────────────────────────────────────────────────────────────

    #[test]
    fn should_point_north_along_minus_z() {
        let road = road_of(&straight_route(10, 10.0, 0.0));
        assert!(road[9].pos[2] < road[0].pos[2], "north must be −z");
        assert!((road[9].pos[0] - road[0].pos[0]).abs() < 1e-3);
    }

    #[test]
    fn should_keep_the_ends_of_the_road_where_the_gpx_put_them() {
        let route = straight_route(20, 10.0, 1.0);
        let road = road_of(&route);
        assert_eq!(road[0].pos[1], 10.0);
        assert_eq!(road[19].pos[1], 29.0);
    }

    #[test]
    fn should_flatten_a_one_point_gps_spike() {
        let mut route = straight_route(21, 10.0, 0.0);
        route.points[10].elevation_m += 7.0;
        let road = road_of(&route);
        // Spread over seven points: a seventh of the spike survives, no more.
        assert!(
            (road[10].pos[1] - (10.0 + 1.0)).abs() < 1e-3,
            "{}",
            road[10].pos[1]
        );
    }

    #[test]
    fn should_carry_the_routes_own_distances_through() {
        let route = straight_route(5, 12.5, 0.0);
        let road = road_of(&route);
        let d: Vec<f32> = road.iter().map(|p| p.distance_m).collect();
        assert_eq!(d, vec![0.0, 12.5, 25.0, 37.5, 50.0]);
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

    #[test]
    fn should_put_the_eye_exactly_eye_height_above_the_road() {
        let road = road_of(&straight_route(50, 10.0, 0.5));
        let pose = camera_pose(&road, 123.0).expect("a road");
        let surface = position_at(&road, 123.0).expect("a road");
        assert_eq!(pose.eye[1], surface[1] + EYE_HEIGHT_M);
    }

    #[test]
    fn should_look_up_the_road() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let pose = camera_pose(&road, 100.0).expect("a road");
        assert!(pose.target[2] < pose.eye[2], "looking north, −z");
        let ahead = pose.eye[2] - pose.target[2];
        assert!((ahead - LOOK_AHEAD_M).abs() < 1e-2, "{ahead}");
    }

    /// A straight road due east from (52.0, 6.0), one point every 10 m.
    fn eastward_route(points: usize) -> Route {
        let frame = LocalFrame::at(52.0, 6.0);
        let pts: Vec<RoutePoint> = (0..points)
            .map(|i| {
                let (lat, lng) = frame.to_geo(i as f64 * 10.0, 0.0);
                RoutePoint {
                    lat,
                    lng,
                    elevation_m: 10.0,
                    distance_m: i as f32 * 10.0,
                    gradient: 0.0,
                }
            })
            .collect();
        Route {
            name: "East".into(),
            total_distance_m: pts.last().map(|p| p.distance_m).unwrap_or(0.0),
            total_gain_m: 0.0,
            points: pts,
        }
    }

    #[test]
    fn should_ride_on_the_gpx_line_whichever_way_the_road_runs() {
        // The GPX is the middle of the rider's lane; the road is built around
        // it (see `roadway`), so the eye is on it, not offset from it.
        for road in [
            road_of(&straight_route(50, 10.0, 0.0)),
            road_of(&eastward_route(50)),
        ] {
            for d in [0.0, 200.0, 490.0, 5_000.0] {
                let pose = camera_pose(&road, d).expect("a road");
                let on = position_at(&road, d).expect("a road");
                assert!((pose.eye[0] - on[0]).abs() < 1e-3, "at {d}: {pose:?}");
                assert!((pose.eye[2] - on[2]).abs() < 1e-3, "at {d}: {pose:?}");
            }
        }
    }

    #[test]
    fn should_look_straight_down_a_straight_road() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let pose = camera_pose(&road, 200.0).expect("a road");
        assert!((pose.target[0] - pose.eye[0]).abs() < 1e-3, "{pose:?}");
    }

    #[test]
    fn should_keep_looking_forward_at_and_past_the_finish() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        for d in [490.0, 500.0, 10_000.0] {
            let pose = camera_pose(&road, d).expect("a road");
            let flat = normalise_xz(sub3(pose.target, pose.eye)).expect("a direction");
            assert!(flat[2] < -0.99, "at {d} m the view turned: {flat:?}");
        }
    }

    #[test]
    fn should_have_a_direction_on_a_road_shorter_than_the_look_ahead() {
        let road = road_of(&straight_route(2, 5.0, 0.0));
        let pose = camera_pose(&road, 0.0).expect("a road");
        assert!(normalise_xz(sub3(pose.target, pose.eye)).is_some());
    }

    #[test]
    fn should_still_aim_somewhere_when_every_point_is_the_same() {
        // A GPX of a rider standing still: one place, many points.
        let mut route = straight_route(10, 0.0, 0.0);
        for (i, p) in route.points.iter_mut().enumerate() {
            p.distance_m = i as f32;
        }
        let road = road_of(&route);
        let pose = camera_pose(&road, 3.0).expect("a road");
        assert!(normalise_xz(sub3(pose.target, pose.eye)).is_some());
    }

    // ── Meshes ────────────────────────────────────────────────────────────────

    #[test]
    fn should_index_only_vertices_that_exist() {
        let scene = CourseScene::build(&straight_route(40, 9.0, 0.3)).expect("a route");
        let n = scene.mesh.vertices.len() as u32;
        assert!(scene.mesh.indices.iter().all(|&i| i < n));
        assert_eq!(scene.mesh.indices.len() % 3, 0);
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
        for v in &ground.vertices {
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
            uv: NO_UV,
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
        for v in &ground.vertices {
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
        let mut row: Vec<[f32; 3]> = ground
            .vertices
            .iter()
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
        for v in &ground.vertices {
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
    fn should_lay_seabed_flat_at_sea_level_and_keep_a_polder_as_land() {
        // A coastal road at sea level: deep water to the west, a polder 1.5 m
        // below sea level to the east.
        let road = road_of(&straight_route(200, 10.0, 0.0));
        let line = resample(&road, RESAMPLE_M);
        let coast = |x: f32, _z: f32| {
            Some(if x < -100.0 {
                -40.0
            } else if x > 100.0 {
                -1.5
            } else {
                10.0
            })
        };
        let ground = ground_mesh(&road, GROUND_SPACING_M, 1_000.0, 0.0, Some(&coast));
        for v in &ground.vertices {
            if brute_nearest(&line, v.pos[0], v.pos[2]) <= LOCAL_FIT_END_M + 1.0 {
                continue;
            }
            if v.pos[0] < -100.0 {
                assert_eq!(v.colour[..3], WATER_RGB, "seabed must read as water");
                assert!((v.pos[1] - 0.0).abs() < 1e-3, "{:?}", v.pos);
            } else if v.pos[0] > 100.0 {
                assert_eq!(v.colour[..3], GROUND_RGB, "a polder is land");
                assert!((v.pos[1] + 1.5).abs() < 1e-3, "{:?}", v.pos);
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

    // ── Materials ─────────────────────────────────────────────────────────────

    #[test]
    fn should_never_roll_a_wall_over_into_the_next_material() {
        for i in -3000..3000 {
            let m = wall_material(i as f32 * 0.001);
            assert!((MATERIAL_WALL..MATERIAL_WALL + 1.0).contains(&m), "{m}");
        }
        // A NaN base (a building on missing ground) still stays a wall.
        assert!(wall_material(f32::NAN) < MATERIAL_WALL + 1.0);
    }

    // ── The route over itself ─────────────────────────────────────────────────

    #[test]
    fn should_never_bury_the_lower_pass_of_a_loop() {
        // North at 0 m, round a loop, back south at 8 m on a line 15 m to the
        // side — close enough that its flat shoulder reaches over the lower
        // road, as at 18.9 km of the rider's Maltepe route.
        let mut line = Vec::new();
        let mut d = 0.0;
        for i in 0..50 {
            line.push(RoadPoint {
                pos: [0.0, 0.0, -(i as f32) * 8.0],
                distance_m: d,
            });
            d += 8.0;
        }
        for i in 0..50 {
            let t = i as f32 / 49.0;
            let a = std::f32::consts::PI * t;
            line.push(RoadPoint {
                pos: [15.0 * t + 60.0 * a.sin(), 8.0 * t, -400.0 - 60.0 * a.sin()],
                distance_m: d,
            });
            d += 8.0;
        }
        for i in 0..50 {
            line.push(RoadPoint {
                pos: [15.0, 8.0, -400.0 + (i as f32) * 8.0],
                distance_m: d,
            });
            d += 8.0;
        }
        let ground = ground_mesh(&line, GROUND_SPACING_M, 200.0, GROUND_DROP_M, None);
        let lower = &line[5..45];
        // The ground only: the upper pass's parapets and pillars are meant to
        // stand above the lower road.
        for v in ground
            .vertices
            .iter()
            .filter(|v| v.colour[3] == MATERIAL_GROUND)
        {
            let near_lower = lower
                .iter()
                .any(|p| (p.pos[0] - v.pos[0]).hypot(p.pos[2] - v.pos[2]) <= SHOULDER_M);
            if near_lower {
                assert!(
                    v.pos[1] <= 0.0 - GROUND_DROP_M + 1e-3,
                    "ground at {:?} stands over the lower road",
                    v.pos
                );
            }
        }
    }
}
