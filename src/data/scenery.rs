//! The map's features in scene space: what colour the ground is, where the
//! sea is, and buildings as triangles.
//!
//! Pure: no GTK, no GL, no network. [`crate::data::osm`] fetches and reads the
//! map; this turns it into what [`crate::data::course3d`] draws.
//!
//! Scene space is the one `course3d` uses: metres, x east, z south.

use std::collections::HashMap;

use crate::data::course3d::{wall_material, LocalFrame, Mesh, Vertex, MATERIAL_PLAIN, NO_UV};
use crate::data::osm::{splitmix, AreaKind, Features};
use crate::data::roofs::{self, BuildingKind, Planned, Region, RoofShape, RoofTags};

/// Ground this low (raw elevation, metres) on the water side of a coastline is
/// sea. The tiles store the inshore Marmara as flat ground at ~0 m; the cap
/// keeps a hill behind a winding shore from being drowned.
pub const SEA_BELOW_M: f32 = 3.0;

/// How far from a coastline its side still decides what the ground is.
const COAST_REACH_M: f32 = 1_500.0;

/// Spatial bucket width for areas and coastline, in metres.
const CELL_M: f32 = 250.0;

/// An area whose bounding box covers more buckets than this is checked
/// everywhere instead of being filed in each — a forest relation the size of
/// a province would otherwise fill the index on its own.
const MAX_CELLS_PER_AREA: i64 = 40_000;

/// A building is drawn sunk this far into the ground at its lowest corner, so
/// on a slope it meets the ground everywhere instead of floating at the low
/// side.
const SINK_M: f32 = 1.0;

/// Width of one window bay, in metres. Each wall gets a whole number of them,
/// so its windows sit centred and none is cut off at a corner.
pub const BAY_M: f32 = 2.8;

/// How many window bays fit a wall `length` metres long; none for a wall too
/// short to hold a window.
pub fn bays(length: f32) -> u32 {
    if length.is_finite() && length >= BAY_M * 0.6 {
        (length / BAY_M).round().max(1.0) as u32
    } else {
        0
    }
}

/// Wall colours, picked per building. Plaster and stone, as Istanbul and the
/// Achterhoek are both mostly built from; linear RGB, fixed like the rest of
/// the scene.
const WALL_RGB: [[f32; 3]; 6] = [
    [0.86, 0.82, 0.74],
    [0.80, 0.76, 0.70],
    [0.88, 0.86, 0.80],
    [0.76, 0.70, 0.62],
    [0.84, 0.78, 0.68],
    [0.72, 0.68, 0.64],
];

/// The colour the ground takes inside an area of `kind`.
pub fn area_rgb(kind: AreaKind) -> [f32; 3] {
    match kind {
        AreaKind::Water => WATER_RGB,
        AreaKind::Wood => [0.20, 0.35, 0.17],
        AreaKind::Farmland => [0.56, 0.58, 0.32],
        AreaKind::Grass => [0.38, 0.56, 0.28],
        AreaKind::Sand => [0.80, 0.74, 0.56],
        AreaKind::Urban => [0.52, 0.51, 0.49],
        AreaKind::Residential => [0.58, 0.57, 0.53],
    }
}

/// Water, wherever it comes from: a lake, a reservoir or the sea.
pub const WATER_RGB: [f32; 3] = [0.20, 0.36, 0.50];

/// A footprint in scene space, without its closing point.
type Ring = Vec<(f32, f32)>;

struct LocalArea {
    kind: AreaKind,
    rings: Vec<Ring>,
    /// x_lo, z_lo, x_hi, z_hi
    bbox: [f32; 4],
}

/// One footprint, ready to raise.
pub struct LocalBuilding {
    pub id: u64,
    pub outline: Ring,
    pub height_m: f32,
    pub height_tagged: bool,
    pub kind: BuildingKind,
    pub roof: RoofTags,
}

/// The map along a route, in scene space and indexed for lookups.
pub struct Scenery {
    areas: Vec<LocalArea>,
    /// Area indices by bucket, in priority order within each bucket.
    area_cells: HashMap<(i64, i64), Vec<usize>>,
    /// Areas too big to file per bucket, checked everywhere.
    area_everywhere: Vec<usize>,
    /// Coastline segments: start, end.
    coast: Vec<((f32, f32), (f32, f32))>,
    coast_cells: HashMap<(i64, i64), Vec<usize>>,
    pub buildings: Vec<LocalBuilding>,
    pub roads: Vec<crate::data::roadway::LocalRoad>,
    /// Where the route is, which decides untagged roofs.
    pub region: Region,
}

fn cell(x: f32, z: f32) -> (i64, i64) {
    ((x / CELL_M).floor() as i64, (z / CELL_M).floor() as i64)
}

/// Every bucket a box touches, or `None` if it touches too many to list.
fn cells_of(bbox: [f32; 4]) -> Option<Vec<(i64, i64)>> {
    let (a, b) = (cell(bbox[0], bbox[1]), cell(bbox[2], bbox[3]));
    let count = (b.0 - a.0 + 1).saturating_mul(b.1 - a.1 + 1);
    if count > MAX_CELLS_PER_AREA {
        return None;
    }
    Some(
        (a.0..=b.0)
            .flat_map(|i| (a.1..=b.1).map(move |j| (i, j)))
            .collect(),
    )
}

fn bbox_of(points: impl Iterator<Item = (f32, f32)>) -> [f32; 4] {
    points.fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, (x, z)| {
        [b[0].min(x), b[1].min(z), b[2].max(x), b[3].max(z)]
    })
}

/// A lat/lng ring as scene-space points, without the closing duplicate.
fn to_scene(frame: &LocalFrame, ring: &[(f64, f64)]) -> Ring {
    let mut out: Ring = ring
        .iter()
        .map(|&(lat, lng)| {
            let (e, n) = frame.to_local(lat, lng);
            (e as f32, -n as f32)
        })
        .collect();
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

impl Scenery {
    /// Place `features` in the scene around `frame`.
    pub fn new(features: &Features, frame: &LocalFrame) -> Self {
        let mut areas: Vec<LocalArea> = features
            .areas
            .iter()
            .filter_map(|a| {
                let rings: Vec<Ring> = a
                    .rings
                    .iter()
                    .map(|r| to_scene(frame, r))
                    .filter(|r| r.len() >= 3)
                    .collect();
                if rings.is_empty() {
                    return None;
                }
                let bbox = bbox_of(rings.iter().flatten().copied());
                Some(LocalArea {
                    kind: a.kind,
                    rings,
                    bbox,
                })
            })
            .collect();
        // Highest priority first, so the first area found containing a point
        // is the one that colours it.
        areas.sort_by_key(|a| a.kind.priority());

        let mut area_cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let mut area_everywhere = Vec::new();
        for (i, a) in areas.iter().enumerate() {
            match cells_of(a.bbox) {
                Some(cells) => cells
                    .into_iter()
                    .for_each(|c| area_cells.entry(c).or_default().push(i)),
                None => area_everywhere.push(i),
            }
        }

        let mut coast = Vec::new();
        let mut coast_cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for way in &features.coast {
            let pts: Vec<(f32, f32)> = way
                .iter()
                .map(|&(lat, lng)| {
                    let (e, n) = frame.to_local(lat, lng);
                    (e as f32, -n as f32)
                })
                .collect();
            for w in pts.windows(2) {
                if w[0] == w[1] {
                    continue;
                }
                let i = coast.len();
                coast.push((w[0], w[1]));
                let bbox = bbox_of([w[0], w[1]].into_iter());
                if let Some(cells) = cells_of(bbox) {
                    cells
                        .into_iter()
                        .for_each(|c| coast_cells.entry(c).or_default().push(i));
                }
            }
        }

        let region = {
            let (lat, lng) = frame.origin();
            Region::at(lat, lng)
        };
        let buildings = features
            .buildings
            .iter()
            .map(|b| {
                let outline = to_scene(frame, &b.outline);
                // Only a guess is replaced: a tagged height or storey count
                // always stands.
                let guessed = !b.height_tagged && !b.levels_tagged;
                let height_m = guessed
                    .then(|| {
                        roofs::regional_walls(b.id, b.kind, region, roofs::footprint_area(&outline))
                    })
                    .flatten()
                    .unwrap_or(b.height_m);
                LocalBuilding {
                    id: b.id,
                    outline,
                    height_m,
                    height_tagged: b.height_tagged,
                    kind: b.kind,
                    roof: b.roof,
                }
            })
            .filter(|b| b.outline.len() >= 3)
            .collect();

        Self {
            areas,
            area_cells,
            area_everywhere,
            coast,
            coast_cells,
            buildings,
            roads: crate::data::roadway::localise(&features.roads, frame),
            region,
        }
    }

    /// The highest-priority area containing `(x, z)`, if any.
    pub fn area_at(&self, x: f32, z: f32) -> Option<AreaKind> {
        let filed = self.area_cells.get(&cell(x, z));
        let candidates = filed
            .into_iter()
            .flatten()
            .chain(self.area_everywhere.iter());
        let mut best: Option<AreaKind> = None;
        for &i in candidates {
            let a = &self.areas[i];
            if best.is_some_and(|b| b.priority() <= a.kind.priority()) {
                continue;
            }
            if x < a.bbox[0] || x > a.bbox[2] || z < a.bbox[1] || z > a.bbox[3] {
                continue;
            }
            if inside(&a.rings, x, z) {
                best = Some(a.kind);
            }
        }
        best
    }

    /// Whether `(x, z)`, whose raw elevation is `raw_m`, is open sea: low, and
    /// on the water side of the nearest coastline within reach.
    pub fn is_sea(&self, x: f32, z: f32, raw_m: f32) -> bool {
        if raw_m >= SEA_BELOW_M || self.coast.is_empty() {
            return false;
        }
        let (kx, kz) = cell(x, z);
        let rings = (COAST_REACH_M / CELL_M).ceil() as i64;
        let mut best: Option<(f32, usize)> = None;
        for dx in -rings..=rings {
            for dz in -rings..=rings {
                let Some(ids) = self.coast_cells.get(&(kx + dx, kz + dz)) else {
                    continue;
                };
                for &i in ids {
                    let d2 = segment_d2(self.coast[i], x, z);
                    if best.is_none_or(|(b, _)| d2 < b) {
                        best = Some((d2, i));
                    }
                }
            }
        }
        let Some((d2, i)) = best else {
            return false;
        };
        if d2 > COAST_REACH_M * COAST_REACH_M {
            return false;
        }
        let ((ax, az), (bx, bz)) = self.coast[i];
        // Land is on the left walking the coastline. In x-east, z-south
        // coordinates the left of (dx, dz) is where this cross product is
        // negative; the sea, positive.
        let cross = (bx - ax) * (z - az) - (bz - az) * (x - ax);
        cross > 0.0
    }
}

/// Squared distance from `(x, z)` to a segment.
fn segment_d2(((ax, az), (bx, bz)): ((f32, f32), (f32, f32)), x: f32, z: f32) -> f32 {
    let (dx, dz) = (bx - ax, bz - az);
    let len2 = dx * dx + dz * dz;
    let t = if len2 > 0.0 {
        (((x - ax) * dx + (z - az) * dz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (ax + t * dx - x).powi(2) + (az + t * dz - z).powi(2)
}

/// Even-odd point-in-polygon over every ring, so inner rings are holes.
fn inside(rings: &[Ring], x: f32, z: f32) -> bool {
    rings
        .iter()
        .fold(false, |odd, r| odd ^ inside_ring(r, x, z))
}

/// Whether `(x, z)` is inside one ring, by counting edge crossings.
fn inside_ring(ring: &[(f32, f32)], x: f32, z: f32) -> bool {
    let n = ring.len();
    let mut odd = false;
    for i in 0..n {
        let (ax, az) = ring[i];
        let (bx, bz) = ring[(i + 1) % n];
        if (az > z) != (bz > z) && x < ax + (z - az) / (bz - az) * (bx - ax) {
            odd = !odd;
        }
    }
    odd
}

/// Whether `(x, z)` lies inside the outline, or within `clearance` of any of
/// its edges.
pub fn near_or_inside(outline: &[(f32, f32)], x: f32, z: f32, clearance: f32) -> bool {
    let n = outline.len();
    let c2 = clearance * clearance;
    (0..n).any(|i| segment_d2((outline[i], outline[(i + 1) % n]), x, z) <= c2)
        || inside_ring(outline, x, z)
}

/// Twice the signed area of a ring: positive when it runs anticlockwise seen
/// from above (x right, z down the screen means clockwise on paper — the sign
/// only has to be consistent).
fn twice_area(ring: &[(f32, f32)]) -> f32 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (ax, az) = ring[i];
            let (bx, bz) = ring[(i + 1) % n];
            ax * bz - bx * az
        })
        .sum()
}

fn cross(o: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// Cut a simple polygon into triangles by ear clipping, as indices into
/// `ring`. `None` for a polygon with no area or one that crosses itself so
/// badly no ear can be found — the caller draws its walls and leaves the roof.
pub fn triangulate(ring: &[(f32, f32)]) -> Option<Vec<[usize; 3]>> {
    let n = ring.len();
    let area = twice_area(ring);
    if n < 3 || !area.is_finite() || area.abs() < 1e-3 {
        return None;
    }
    // Work anticlockwise (positive area) whatever way the mapper drew it.
    let mut left: Vec<usize> = if area > 0.0 {
        (0..n).collect()
    } else {
        (0..n).rev().collect()
    };
    let mut out = Vec::with_capacity(n - 2);
    let mut misses = 0;
    let mut i = 0;
    while left.len() > 3 {
        if misses > left.len() {
            return None; // a full lap with no ear: the polygon crosses itself
        }
        let m = left.len();
        let (pi, ci, ni) = (left[(i + m - 1) % m], left[i % m], left[(i + 1) % m]);
        let (p, c, nx) = (ring[pi], ring[ci], ring[ni]);
        let convex = cross(p, c, nx) > 1e-9;
        let empty = convex
            && left.iter().all(|&k| {
                if k == pi || k == ci || k == ni {
                    return true;
                }
                let q = ring[k];
                // On or inside the candidate ear blocks it; a repeated corner
                // sitting on one of its points does too.
                !(cross(p, c, q) >= 0.0 && cross(c, nx, q) >= 0.0 && cross(nx, p, q) >= 0.0)
            });
        if convex && empty {
            out.push([pi, ci, ni]);
            left.remove(i % m);
            misses = 0;
            if i >= left.len() {
                i = 0;
            }
        } else {
            i = (i + 1) % m;
            misses += 1;
        }
    }
    out.push([left[0], left[1], left[2]]);
    Some(out)
}

/// The buildings as triangles, standing on the ground.
///
/// `ground(x, z)` is the drawn ground's height there; `clear(x, z)` is true
/// where a building must not stand — on or beside the road. A building with any
/// corner, or any part of its outline, on such ground is left out: a missing
/// building is far less wrong than one standing on the tarmac.
///
/// Each roof is planned by [`roofs::plan`]: the walls stop at the eaves, and a
/// pitched roof — with its gable walls — sits on the building's rectangle.
pub fn building_mesh(
    buildings: &[LocalBuilding],
    region: Region,
    ground: impl Fn(f32, f32) -> Option<f32>,
    blocked: impl Fn(&[(f32, f32)]) -> bool,
) -> Mesh {
    let mut mesh = Mesh::default();
    for b in buildings {
        if blocked(&b.outline) {
            continue;
        }
        let Some(lowest) = b
            .outline
            .iter()
            .map(|&(x, z)| ground(x, z))
            .try_fold(f32::MAX, |lo, h| h.map(|h| lo.min(h)))
        else {
            continue; // part of it is off the ground the view draws
        };
        if !b.height_m.is_finite() || b.height_m <= 0.0 {
            continue;
        }
        let plan = roofs::plan(
            Planned {
                id: b.id,
                outline: &b.outline,
                kind: b.kind,
                height_m: b.height_m,
                height_tagged: b.height_tagged,
                roof: b.roof,
            },
            region,
        );
        let base = lowest - SINK_M;
        let top = lowest + plan.wall_m;
        let hash = splitmix(b.id);
        let wall = WALL_RGB[(hash % WALL_RGB.len() as u64) as usize];
        // The window pattern's seed: any fraction, the same every ride.
        let seed = (hash >> 40) as f32 / (1u64 << 24) as f32;
        let gable = [wall[0], wall[1], wall[2], MATERIAL_PLAIN];
        let wall = [wall[0], wall[1], wall[2], wall_material(seed)];
        let roof = [
            plan.colour[0],
            plan.colour[1],
            plan.colour[2],
            MATERIAL_PLAIN,
        ];

        let n = b.outline.len();
        for i in 0..n {
            let (ax, az) = b.outline[i];
            let (bx, bz) = b.outline[(i + 1) % n];
            let v = mesh.vertices.len() as u32;
            let n_bays = bays(((bx - ax).powi(2) + (bz - az).powi(2)).sqrt());
            // Along: 0 → bay count, or −1 throughout for no windows. Up:
            // metres from the building's ground, so its storeys are its own.
            let (u0, u1) = if n_bays == 0 {
                (-1.0, -1.0)
            } else {
                (0.0, n_bays as f32)
            };
            let (v0, v1) = (-SINK_M, plan.wall_m);
            for (pos, uv) in [
                ([ax, base, az], [u0, v0]),
                ([bx, base, bz], [u1, v0]),
                ([bx, top, bz], [u1, v1]),
                ([ax, top, az], [u0, v1]),
            ] {
                mesh.vertices.push(Vertex {
                    pos,
                    colour: wall,
                    uv,
                });
            }
            mesh.indices
                .extend_from_slice(&[v, v + 1, v + 2, v, v + 2, v + 3]);
        }
        if plan.shape != RoofShape::Flat {
            mesh.extend(roofs::roof_mesh(&plan, top, gable));
        } else if let Some(tris) = triangulate(&b.outline) {
            let v = mesh.vertices.len() as u32;
            for &(x, z) in &b.outline {
                mesh.vertices.push(Vertex {
                    pos: [x, top, z],
                    colour: roof,
                    uv: NO_UV,
                });
            }
            for t in tris {
                mesh.indices.extend_from_slice(&[
                    v + t[0] as u32,
                    v + t[1] as u32,
                    v + t[2] as u32,
                ]);
            }
        }
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::osm::{Area, Building};

    fn tri_area(ring: &[(f32, f32)], t: [usize; 3]) -> f32 {
        cross(ring[t[0]], ring[t[1]], ring[t[2]]).abs() / 2.0
    }

    // ── Triangulation ─────────────────────────────────────────────────────────

    #[test]
    fn should_cut_a_square_into_two_triangles_either_way_round() {
        let sq = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        for ring in [sq.to_vec(), sq.iter().rev().copied().collect::<Vec<_>>()] {
            let tris = triangulate(&ring).expect("a square");
            assert_eq!(tris.len(), 2);
            let total: f32 = tris.iter().map(|&t| tri_area(&ring, t)).sum();
            assert!((total - 100.0).abs() < 1e-3);
        }
    }

    #[test]
    fn should_cut_a_concave_l_without_covering_the_notch() {
        // An L: 20×20 with the top-right 10×10 missing. Area 300.
        let l = [
            (0.0, 0.0),
            (20.0, 0.0),
            (20.0, 10.0),
            (10.0, 10.0),
            (10.0, 20.0),
            (0.0, 20.0),
        ];
        let tris = triangulate(&l).expect("an L");
        assert_eq!(tris.len(), 4);
        let total: f32 = tris.iter().map(|&t| tri_area(&l, t)).sum();
        assert!(
            (total - 300.0).abs() < 1e-3,
            "{total}: the notch was covered"
        );
    }

    #[test]
    fn should_leave_a_flat_or_tiny_footprint_unroofed() {
        assert_eq!(triangulate(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]), None);
        assert_eq!(triangulate(&[(0.0, 0.0), (1.0, 1.0)]), None);
        assert_eq!(triangulate(&[]), None);
        assert_eq!(
            triangulate(&[(0.0, 0.0), (f32::NAN, 1.0), (1.0, 0.0)]),
            None
        );
    }

    #[test]
    fn should_give_up_on_a_bow_tie_rather_than_loop() {
        // Crosses itself; whatever comes back must terminate.
        let bow = [(0.0, 0.0), (10.0, 10.0), (10.0, 0.0), (0.0, 10.0)];
        let _ = triangulate(&bow);
    }

    proptest::proptest! {
        #[test]
        fn should_always_finish_and_index_only_real_corners(
            pts in proptest::collection::vec((-50.0f32..50.0, -50.0f32..50.0), 0..14)
        ) {
            if let Some(tris) = triangulate(&pts) {
                for t in tris { for i in t { proptest::prop_assert!(i < pts.len()); } }
            }
        }
    }

    // ── Areas ─────────────────────────────────────────────────────────────────

    fn frame() -> LocalFrame {
        LocalFrame::at(41.0, 29.0)
    }

    /// A lat/lng square `size` metres across, south-west corner `e`, `n`
    /// metres from the frame's origin.
    fn geo_square(e: f64, n: f64, size: f64) -> Vec<(f64, f64)> {
        let f = frame();
        let mut r: Vec<(f64, f64)> = [(e, n), (e + size, n), (e + size, n + size), (e, n + size)]
            .iter()
            .map(|&(a, b)| f.to_geo(a, b))
            .collect();
        r.push(r[0]);
        r
    }

    fn scenery(features: Features) -> Scenery {
        Scenery::new(&features, &frame())
    }

    #[test]
    fn should_colour_ground_by_the_area_it_is_in() {
        let s = scenery(Features {
            areas: vec![Area {
                kind: AreaKind::Wood,
                rings: vec![geo_square(0.0, 0.0, 100.0)],
            }],
            ..Default::default()
        });
        // Scene z is south, so 50 m north is z = −50.
        assert_eq!(s.area_at(50.0, -50.0), Some(AreaKind::Wood));
        assert_eq!(s.area_at(150.0, -50.0), None);
        assert_eq!(s.area_at(50.0, 50.0), None);
    }

    #[test]
    fn should_leave_an_island_out_of_its_lake() {
        let s = scenery(Features {
            areas: vec![Area {
                kind: AreaKind::Water,
                rings: vec![geo_square(0.0, 0.0, 300.0), geo_square(100.0, 100.0, 100.0)],
            }],
            ..Default::default()
        });
        assert_eq!(s.area_at(50.0, -50.0), Some(AreaKind::Water));
        assert_eq!(s.area_at(150.0, -150.0), None, "the island is land");
    }

    #[test]
    fn should_let_a_pond_win_over_the_park_around_it_in_any_order() {
        for flip in [false, true] {
            let mut areas = vec![
                Area {
                    kind: AreaKind::Grass,
                    rings: vec![geo_square(0.0, 0.0, 500.0)],
                },
                Area {
                    kind: AreaKind::Water,
                    rings: vec![geo_square(200.0, 200.0, 50.0)],
                },
            ];
            if flip {
                areas.reverse();
            }
            let s = scenery(Features {
                areas,
                ..Default::default()
            });
            assert_eq!(s.area_at(225.0, -225.0), Some(AreaKind::Water));
            assert_eq!(s.area_at(100.0, -100.0), Some(AreaKind::Grass));
        }
    }

    #[test]
    fn should_still_find_an_area_too_big_to_index() {
        // 60 km across: past the per-area bucket limit, checked everywhere.
        let s = scenery(Features {
            areas: vec![Area {
                kind: AreaKind::Farmland,
                rings: vec![geo_square(-30_000.0, -30_000.0, 60_000.0)],
            }],
            ..Default::default()
        });
        assert_eq!(s.area_at(0.0, 0.0), Some(AreaKind::Farmland));
    }

    // ── Sea ───────────────────────────────────────────────────────────────────

    /// A coastline running due east along the frame's origin: land on the
    /// left of east is north, so the sea is to the south.
    fn east_coast() -> Scenery {
        let f = frame();
        scenery(Features {
            coast: vec![vec![f.to_geo(-2_000.0, 0.0), f.to_geo(2_000.0, 0.0)]],
            ..Default::default()
        })
    }

    #[test]
    fn should_put_the_sea_on_the_right_of_the_coastline() {
        let s = east_coast();
        // 100 m south (z = +100) is sea; 100 m north is land.
        assert!(s.is_sea(0.0, 100.0, 0.5));
        assert!(!s.is_sea(0.0, -100.0, 0.5));
    }

    #[test]
    fn should_not_drown_high_ground_on_the_water_side() {
        // A headland beyond a bend, or a cliff: high ground stays land.
        assert!(!east_coast().is_sea(0.0, 100.0, SEA_BELOW_M));
        assert!(east_coast().is_sea(0.0, 100.0, SEA_BELOW_M - 0.01));
    }

    #[test]
    fn should_know_no_sea_without_a_coastline_nearby() {
        assert!(!east_coast().is_sea(0.0, 5_000.0, 0.0));
        assert!(!scenery(Features::default()).is_sea(0.0, 100.0, 0.0));
    }

    #[test]
    fn should_read_a_reversed_coastline_the_other_way() {
        // Drawn westwards: the land is now south.
        let f = frame();
        let s = scenery(Features {
            coast: vec![vec![f.to_geo(2_000.0, 0.0), f.to_geo(-2_000.0, 0.0)]],
            ..Default::default()
        });
        assert!(s.is_sea(0.0, -100.0, 0.5));
        assert!(!s.is_sea(0.0, 100.0, 0.5));
    }

    // ── Buildings ─────────────────────────────────────────────────────────────

    fn one_building(height_m: f32) -> Scenery {
        scenery(Features {
            buildings: vec![Building {
                id: 42,
                outline: geo_square(100.0, 100.0, 20.0),
                height_m,
                height_tagged: false,
                levels_tagged: false,
                kind: BuildingKind::Other,
                roof: RoofTags::default(),
            }],
            ..Default::default()
        })
    }

    #[test]
    fn should_stand_a_building_on_its_lowest_corner_sunk_a_little() {
        let s = one_building(12.0);
        // Ground sloping up to the east.
        let mesh = building_mesh(
            &s.buildings,
            Region::Elsewhere,
            |x, _| Some(x * 0.1),
            |_| false,
        );
        let lo = mesh
            .vertices
            .iter()
            .map(|v| v.pos[1])
            .fold(f32::MAX, f32::min);
        let hi = mesh
            .vertices
            .iter()
            .map(|v| v.pos[1])
            .fold(f32::MIN, f32::max);
        // Lowest corner at x = 100 → ground 10.
        assert!((lo - (10.0 - SINK_M)).abs() < 1e-3, "{lo}");
        assert!((hi - (10.0 + 12.0)).abs() < 1e-3, "{hi}");
    }

    #[test]
    fn should_raise_four_walls_and_a_roof() {
        let s = one_building(6.0);
        let mesh = building_mesh(&s.buildings, Region::Elsewhere, |_, _| Some(0.0), |_| false);
        // Four walls of two triangles, a square roof of two.
        assert_eq!(mesh.indices.len() / 3, 4 * 2 + 2);
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
        // Low, so a tiled roof.
        // An untagged building of no kind in particular, outside the two
        // regions with their own rules: a flat roof.
        assert!(mesh
            .vertices
            .iter()
            .any(|v| v.colour[..3] == roofs::FLAT_ROOF_RGB));
    }

    #[test]
    fn should_leave_out_a_building_on_the_road() {
        let s = one_building(9.0);
        let mesh = building_mesh(&s.buildings, Region::Elsewhere, |_, _| Some(0.0), |_| true);
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn should_leave_out_a_building_partly_off_the_drawn_ground() {
        let s = one_building(9.0);
        let mesh = building_mesh(
            &s.buildings,
            Region::Elsewhere,
            |x, _| (x < 110.0).then_some(0.0),
            |_| false,
        );
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn should_leave_out_a_building_with_no_height_to_it() {
        for h in [0.0, -5.0, f32::NAN] {
            let s = one_building(h);
            assert!(
                building_mesh(&s.buildings, Region::Elsewhere, |_, _| Some(0.0), |_| false)
                    .vertices
                    .is_empty()
            );
        }
    }

    // ── Windows ───────────────────────────────────────────────────────────────

    #[test]
    fn should_fit_a_whole_number_of_bays_to_a_wall() {
        assert_eq!(bays(BAY_M), 1);
        assert_eq!(bays(2.0 * BAY_M), 2);
        assert_eq!(bays(2.4 * BAY_M), 2);
        assert_eq!(bays(2.6 * BAY_M), 3);
        // A wall just long enough for one squeezed window gets it; shorter, none.
        assert_eq!(bays(BAY_M * 0.6), 1);
        assert_eq!(bays(BAY_M * 0.59), 0);
        assert_eq!(bays(0.0), 0);
        assert_eq!(bays(f32::NAN), 0);
    }

    #[test]
    fn should_lay_wall_coordinates_from_corner_to_corner_and_ground_to_roof() {
        // A 20 m square: each wall 20 m, so 7 bays (20 / 2.8 = 7.1).
        let s = one_building(12.0);
        let mesh = building_mesh(&s.buildings, Region::Elsewhere, |_, _| Some(5.0), |_| false);
        let walls: Vec<&Vertex> = mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] >= crate::data::course3d::MATERIAL_WALL)
            .collect();
        assert_eq!(walls.len(), 16);
        let us: std::collections::BTreeSet<i32> = walls.iter().map(|v| v.uv[0] as i32).collect();
        assert_eq!(us, [0, 7].into_iter().collect());
        let vs: std::collections::BTreeSet<i32> = walls
            .iter()
            .map(|v| (v.uv[1] * 10.0).round() as i32)
            .collect();
        assert_eq!(
            vs,
            [-10, 120].into_iter().collect(),
            "−1 m to the 12 m roof"
        );
    }

    #[test]
    fn should_give_a_short_wall_no_windows() {
        let f = frame();
        let tiny = vec![
            f.to_geo(0.0, 0.0),
            f.to_geo(1.0, 0.0),
            f.to_geo(1.0, 1.0),
            f.to_geo(0.0, 1.0),
            f.to_geo(0.0, 0.0),
        ];
        let s = scenery(Features {
            buildings: vec![Building {
                id: 1,
                outline: tiny,
                height_m: 3.0,
                height_tagged: false,
                levels_tagged: false,
                kind: BuildingKind::Other,
                roof: RoofTags::default(),
            }],
            ..Default::default()
        });
        let mesh = building_mesh(&s.buildings, Region::Elsewhere, |_, _| Some(0.0), |_| false);
        assert!(mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] >= crate::data::course3d::MATERIAL_WALL)
            .all(|v| v.uv[0] < 0.0));
    }

    // ── Roofs ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_gable_a_dutch_house_and_stop_its_walls_at_the_eaves() {
        let s = scenery(Features {
            buildings: vec![Building {
                id: 3,
                outline: geo_square(100.0, 100.0, 9.0),
                height_m: 6.0,
                height_tagged: false,
                levels_tagged: false,
                kind: BuildingKind::House,
                roof: RoofTags::default(),
            }],
            ..Default::default()
        });
        let mesh = building_mesh(
            &s.buildings,
            Region::Netherlands,
            |_, _| Some(10.0),
            |_| false,
        );
        let walls: Vec<&Vertex> = mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] >= crate::data::course3d::MATERIAL_WALL)
            .collect();
        // The windowed walls stop at the eaves: ground 10 + walls 6.
        let eaves = walls.iter().map(|v| v.pos[1]).fold(f32::MIN, f32::max);
        assert!((eaves - 16.0).abs() < 1e-3, "{eaves}");
        // Above them, a roof rises to a ridge.
        let top = mesh
            .vertices
            .iter()
            .map(|v| v.pos[1])
            .fold(f32::MIN, f32::max);
        assert!(top > 17.0, "no pitched roof: top {top}");
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
    }
}
