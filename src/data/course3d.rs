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

use crate::data::route::Route;

/// Height of the rider's eye above the road surface, in metres. A rider on
/// the hoods of a road bike, not someone standing at the roadside — the rider
/// found 1.6 m sat noticeably too high.
pub const EYE_HEIGHT_M: f32 = 1.3;

/// How far up the road the camera aims, in metres. Far enough that a GPS
/// wobble in the next point or two does not swing the view, near enough that
/// the camera still turns into a bend rather than across it.
pub const LOOK_AHEAD_M: f32 = 30.0;

/// How far right of the centre line the rider rides, in metres: the middle of
/// the right-hand lane, as on the roads these routes were recorded on.
pub const LANE_OFFSET_M: f32 = 1.5;

/// Half the width of the road surface, in metres.
pub const ROAD_HALF_WIDTH_M: f32 = 3.0;

/// GPX points either side averaged into each centreline point. A recorded ride
/// is about one point every 8–9 m with a metre or two of GPS scatter, which
/// drawn raw reads as a road with a kink every few metres.
const SMOOTH_POINTS: usize = 3;

/// Centre-line dashes: painted this long, then a gap of the rest of the period.
/// The dashes are the main thing that tells the eye how fast the road is going.
const DASH_M: f32 = 3.0;
const DASH_PERIOD_M: f32 = 9.0;
const DASH_HALF_WIDTH_M: f32 = 0.08;

// The rider's line must be on the tarmac and clear of the paint. Checked when
// the crate compiles, so retuning one width cannot quietly break it.
const _: () = assert!(LANE_OFFSET_M > DASH_HALF_WIDTH_M && LANE_OFFSET_M < ROAD_HALF_WIDTH_M);

/// The ground grid: vertex spacing, how far either side of the road it reaches,
/// and how far below the road it sits so the road surface always wins the depth
/// test even where the grid's straight edges cut under a bend.
pub const GROUND_SPACING_M: f32 = 20.0;
pub const GROUND_RADIUS_M: f32 = 600.0;
pub const GROUND_DROP_M: f32 = 0.3;

/// Surface lift of the road and its paint above the centreline, in metres —
/// separate layers so they never fight each other in the depth buffer.
const ROAD_LIFT_M: f32 = 0.0;
const PAINT_LIFT_M: f32 = 0.02;

/// What a vertex is made of. Sent to the shader in the colour's alpha, so one
/// vertex layout carries everything.
pub const MATERIAL_PLAIN: f32 = 0.0;
pub const MATERIAL_GROUND: f32 = 1.0;

/// Scene colours, in linear RGB. Fixed rather than theme-derived, the same as
/// the OSM tiles under the 2D map: a road is grey at night in dark mode too.
const ROAD_RGB: [f32; 3] = [0.30, 0.31, 0.33];
const PAINT_RGB: [f32; 3] = [0.92, 0.92, 0.88];
const GROUND_RGB: [f32; 3] = [0.36, 0.52, 0.27];

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

    /// `(east, north)` in metres from the origin.
    pub fn to_local(self, lat: f64, lng: f64) -> (f64, f64) {
        (
            (lng - self.lng0) * self.m_per_deg_lng,
            (lat - self.lat0) * self.m_per_deg_lat,
        )
    }

    /// `(lat, lng)` of a point `east`, `north` metres from the origin.
    #[cfg(test)]
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
    /// Build the scene for `route`: the road, its paint, and the ground.
    /// `None` for a route too short to draw — fewer than two points.
    pub fn build(route: &Route) -> Option<Self> {
        let frame = LocalFrame::around(route)?;
        let road = centreline(route, &frame);
        if road.len() < 2 {
            return None;
        }
        let mut mesh = ground_mesh(&road, GROUND_SPACING_M, GROUND_RADIUS_M, GROUND_DROP_M);
        mesh.extend(road_mesh(&road, ROAD_HALF_WIDTH_M));
        mesh.extend(dash_mesh(&road));
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

    // Both ends move into the lane, each by the road's direction where it is,
    // so the view runs down the lane rather than angling across the paint.
    let here = in_lane(here, heading_at(road, d));
    let ahead = in_lane(ahead, heading_at(road, ahead_d.min(end)));

    Some(CameraPose {
        eye: [here[0], here[1] + EYE_HEIGHT_M, here[2]],
        // Aimed slightly below eye level, as a rider looks at the road.
        target: [ahead[0], ahead[1] + EYE_HEIGHT_M * 0.6, ahead[2]],
    })
}

/// Half the stretch of road the direction of travel is measured over, in
/// metres — long enough to ride over a single GPS wobble.
const HEADING_SPAN_M: f32 = 5.0;

/// Direction of travel at `distance_m`, flat and of length 1. North when the
/// road gives no direction at all.
fn heading_at(road: &[RoadPoint], distance_m: f32) -> [f32; 3] {
    let (Some(start), Some(end)) = (road.first(), road.last()) else {
        return [0.0, 0.0, -1.0];
    };
    let behind = position_at(road, (distance_m - HEADING_SPAN_M).max(start.distance_m));
    let ahead = position_at(road, (distance_m + HEADING_SPAN_M).min(end.distance_m));
    match (behind, ahead) {
        (Some(b), Some(a)) => normalise_xz(sub3(a, b)).unwrap_or([0.0, 0.0, -1.0]),
        _ => [0.0, 0.0, -1.0],
    }
}

/// `p` moved [`LANE_OFFSET_M`] to the right of a rider heading along `dir`.
fn in_lane(p: [f32; 3], dir: [f32; 3]) -> [f32; 3] {
    // Right of the direction of travel, in the ground plane: the mirror of the
    // `left` the road ribbon is built with.
    let right = [-dir[2], 0.0, dir[0]];
    [
        p[0] + right[0] * LANE_OFFSET_M,
        p[1],
        p[2] + right[2] * LANE_OFFSET_M,
    ]
}

/// The road surface as a ribbon `half_width` either side of the centreline.
pub fn road_mesh(road: &[RoadPoint], half_width: f32) -> Mesh {
    ribbon(road, half_width, ROAD_LIFT_M, ROAD_RGB, |_| true)
}

/// Dashed centre line: painted for [`DASH_M`] of every [`DASH_PERIOD_M`].
fn dash_mesh(road: &[RoadPoint]) -> Mesh {
    ribbon(road, DASH_HALF_WIDTH_M, PAINT_LIFT_M, PAINT_RGB, |d| {
        d.rem_euclid(DASH_PERIOD_M) < DASH_M
    })
}

/// A strip along the centreline. A segment is drawn when `keep` accepts the
/// distance at its start, which is how the dashes are cut from the same strip.
fn ribbon(
    road: &[RoadPoint],
    half_width: f32,
    lift: f32,
    rgb: [f32; 3],
    keep: impl Fn(f32) -> bool,
) -> Mesh {
    let mut mesh = Mesh::default();
    if road.len() < 2 {
        return mesh;
    }
    let colour = [rgb[0], rgb[1], rgb[2], MATERIAL_PLAIN];
    for (i, p) in road.iter().enumerate() {
        let prev = road[i.saturating_sub(1)].pos;
        let next = road[(i + 1).min(road.len() - 1)].pos;
        // Perpendicular to the direction of travel, in the ground plane.
        let dir = normalise_xz(sub3(next, prev)).unwrap_or([0.0, 0.0, -1.0]);
        let left = [dir[2], 0.0, -dir[0]];
        let y = p.pos[1] + lift;
        mesh.vertices.push(Vertex {
            pos: [
                p.pos[0] + left[0] * half_width,
                y,
                p.pos[2] + left[2] * half_width,
            ],
            colour,
        });
        mesh.vertices.push(Vertex {
            pos: [
                p.pos[0] - left[0] * half_width,
                y,
                p.pos[2] - left[2] * half_width,
            ],
            colour,
        });
    }
    for (i, p) in road[..road.len() - 1].iter().enumerate() {
        if !keep(p.distance_m) {
            continue;
        }
        let a = (2 * i) as u32;
        mesh.indices
            .extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
    }
    mesh
}

/// A ground grid reaching `radius` either side of the road, each vertex at the
/// height of the nearest road point less `drop`.
///
/// Until real terrain arrives this is the road's own landscape: flat across,
/// rising and falling with the road. Cells whose corners are not all within
/// reach are left out, so the ground ends in a ragged edge the fog hides.
pub fn ground_mesh(road: &[RoadPoint], spacing: f32, radius: f32, drop: f32) -> Mesh {
    let mut mesh = Mesh::default();
    if road.is_empty() || spacing <= 0.0 || radius <= 0.0 {
        return mesh;
    }
    let index = NearestIndex::new(road, radius);

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

    let colour = [GROUND_RGB[0], GROUND_RGB[1], GROUND_RGB[2], MATERIAL_GROUND];
    let mut slot: Vec<Option<u32>> = vec![None; cols * rows];
    for r in 0..rows {
        for c in 0..cols {
            let x = (i_lo + c as i64) as f32 * spacing;
            let z = (j_lo + r as i64) as f32 * spacing;
            if let Some(y) = index.nearest_height(x, z) {
                slot[r * cols + c] = Some(mesh.vertices.len() as u32);
                mesh.vertices.push(Vertex {
                    pos: [x, y - drop, z],
                    colour,
                });
            }
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
    mesh
}

/// Road points bucketed on a coarse grid, for "nearest road point within
/// reach" without testing every point for every ground vertex.
struct NearestIndex<'a> {
    road: &'a [RoadPoint],
    cell: f32,
    reach: f32,
    buckets: std::collections::HashMap<(i64, i64), Vec<usize>>,
}

impl<'a> NearestIndex<'a> {
    fn new(road: &'a [RoadPoint], reach: f32) -> Self {
        let cell = reach.max(1.0);
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
        }
    }

    fn key(cell: f32, x: f32, z: f32) -> (i64, i64) {
        ((x / cell).floor() as i64, (z / cell).floor() as i64)
    }

    /// Height of the nearest road point within reach of `(x, z)`, if any.
    fn nearest_height(&self, x: f32, z: f32) -> Option<f32> {
        let (kx, kz) = Self::key(self.cell, x, z);
        let mut best: Option<(f32, f32)> = None;
        for dx in -1..=1 {
            for dz in -1..=1 {
                let Some(ids) = self.buckets.get(&(kx + dx, kz + dz)) else {
                    continue;
                };
                for &i in ids {
                    let p = self.road[i].pos;
                    let d2 = (p[0] - x).powi(2) + (p[2] - z).powi(2);
                    if best.is_none_or(|(b, _)| d2 < b) {
                        best = Some((d2, p[1]));
                    }
                }
            }
        }
        best.filter(|(d2, _)| *d2 <= self.reach * self.reach)
            .map(|(_, y)| y)
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
    fn should_ride_in_the_right_hand_lane_heading_north() {
        // Facing north (−z), right is east (+x).
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let pose = camera_pose(&road, 200.0).expect("a road");
        let centre = position_at(&road, 200.0).expect("a road");
        assert!(
            (pose.eye[0] - (centre[0] + LANE_OFFSET_M)).abs() < 1e-3,
            "{pose:?}"
        );
        assert!((pose.eye[2] - centre[2]).abs() < 1e-3, "{pose:?}");
    }

    #[test]
    fn should_ride_in_the_right_hand_lane_heading_east() {
        // Facing east (+x), right is south (+z).
        let road = road_of(&eastward_route(50));
        let pose = camera_pose(&road, 200.0).expect("a road");
        let centre = position_at(&road, 200.0).expect("a road");
        assert!(
            (pose.eye[2] - (centre[2] + LANE_OFFSET_M)).abs() < 1e-3,
            "{pose:?}"
        );
        assert!((pose.eye[0] - centre[0]).abs() < 1e-3, "{pose:?}");
    }

    #[test]
    fn should_look_down_its_own_lane_not_across_to_the_centre_line() {
        // On a straight road the view runs parallel to the road.
        let road = road_of(&straight_route(50, 10.0, 0.0));
        let pose = camera_pose(&road, 200.0).expect("a road");
        assert!((pose.target[0] - pose.eye[0]).abs() < 1e-3, "{pose:?}");
    }

    #[test]
    fn should_stay_in_lane_at_the_start_and_past_the_finish() {
        let road = road_of(&straight_route(50, 10.0, 0.0));
        for d in [0.0, 490.0, 5_000.0] {
            let pose = camera_pose(&road, d).expect("a road");
            let centre = position_at(&road, d).expect("a road");
            assert!(
                (pose.eye[0] - (centre[0] + LANE_OFFSET_M)).abs() < 1e-3,
                "at {d} m: {pose:?}"
            );
            assert!(
                (pose.target[0] - pose.eye[0]).abs() < 1e-3,
                "at {d} m: {pose:?}"
            );
        }
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
    fn should_make_the_road_exactly_its_width_across() {
        let road = road_of(&straight_route(10, 10.0, 0.0));
        let mesh = road_mesh(&road, ROAD_HALF_WIDTH_M);
        let (l, r) = (mesh.vertices[8].pos, mesh.vertices[9].pos);
        let width = ((l[0] - r[0]).powi(2) + (l[2] - r[2]).powi(2)).sqrt();
        assert!((width - 2.0 * ROAD_HALF_WIDTH_M).abs() < 1e-3, "{width}");
    }

    #[test]
    fn should_index_only_vertices_that_exist() {
        let scene = CourseScene::build(&straight_route(40, 9.0, 0.3)).expect("a route");
        let n = scene.mesh.vertices.len() as u32;
        assert!(scene.mesh.indices.iter().all(|&i| i < n));
        assert_eq!(scene.mesh.indices.len() % 3, 0);
    }

    #[test]
    fn should_paint_a_third_of_the_centre_line() {
        // 900 m of road: 100 periods, each painted for 3 of its 9 metres.
        let road = road_of(&straight_route(901, 1.0, 0.0));
        let dashes = dash_mesh(&road);
        let painted = dashes.indices.len() / 6;
        assert_eq!(painted, 300);
    }

    #[test]
    fn should_keep_the_ground_below_the_road_everywhere_along_it() {
        // Climbing 0.4 m every 10 m. Every ground vertex sits under the
        // nearest road point, so none can rise through the road surface.
        let road = road_of(&straight_route(200, 10.0, 0.4));
        let ground = ground_mesh(&road, GROUND_SPACING_M, 100.0, GROUND_DROP_M);
        assert!(!ground.vertices.is_empty());
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
        let ground = ground_mesh(&road, 10.0, 100.0, 0.0);
        let x0 = road[0].pos[0];
        for v in &ground.vertices {
            assert!((v.pos[0] - x0).abs() <= 100.0 + 1e-3, "{:?}", v.pos);
        }
    }

    #[test]
    fn should_build_the_same_ground_twice() {
        let road = road_of(&straight_route(80, 10.0, 0.2));
        let a = ground_mesh(&road, GROUND_SPACING_M, 200.0, GROUND_DROP_M);
        let b = ground_mesh(&road, GROUND_SPACING_M, 200.0, GROUND_DROP_M);
        assert_eq!(a, b);
    }

    #[test]
    fn should_build_no_ground_from_nonsense_settings() {
        let road = road_of(&straight_route(10, 10.0, 0.0));
        assert!(ground_mesh(&road, 0.0, 100.0, 0.0).vertices.is_empty());
        assert!(ground_mesh(&road, 10.0, -1.0, 0.0).vertices.is_empty());
        assert!(ground_mesh(&[], 10.0, 100.0, 0.0).vertices.is_empty());
    }

    #[test]
    fn should_shift_appended_indices_past_existing_vertices() {
        let mut a = road_mesh(&road_of(&straight_route(3, 10.0, 0.0)), 1.0);
        let before = a.vertices.len() as u32;
        let b = road_mesh(&road_of(&straight_route(3, 10.0, 0.0)), 1.0);
        let first_b = b.indices[0];
        a.extend(b);
        let tail = a.indices.len() - 12;
        assert_eq!(a.indices[tail], first_b + before);
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
}
