//! The road itself: how wide it is, where its lanes are, and the other roads
//! around it — from OpenStreetMap, matched to the route.
//!
//! The GPX is the line the rider rode, and the rider rides the rightmost lane
//! (the rider's own answer). So the road is built *around* that line: its
//! right edge half a lane to the right, its lanes stacked to the left, as many
//! as the map says the road under the route has. Where the map has nothing to
//! say, it is the two-lane road the view has always drawn.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use std::collections::HashMap;

use crate::data::course3d::{
    LocalFrame, Mesh, RoadPoint, Vertex, MATERIAL_PLAIN, MATERIAL_ROAD, NO_UV,
};
use crate::data::osm::{Road, RoadKind};

/// How far from the GPX a mapped road may lie and still be the road ridden.
/// GPS and the map each put a road a few metres off; a parallel road is
/// usually further than this.
const MATCH_M: f32 = 18.0;

/// A mapped road has to run within this many degrees of the direction of
/// travel to be the one ridden. A one-way road running the other way is the
/// opposite carriageway, never a match.
const MATCH_ANGLE_DEG: f32 = 35.0;

/// Points either side whose matches are pooled, so one GPS wobble towards a
/// side street does not change the road for a few metres.
const MATCH_SMOOTH: usize = 4;

/// Points either side the edges of the road are averaged over, so a change of
/// lane count widens the road over tens of metres rather than in a step.
const EDGE_SMOOTH: usize = 6;

/// Extra distance charged to matching a cycleway or service road, in metres:
/// the rider rides the road, and a cycle path alongside it is often nearer
/// the GPS trace than the road's centreline is.
const CYCLEWAY_PENALTY_M: f32 = 8.0;
const SERVICE_PENALTY_M: f32 = 4.0;
/// Slip roads run alongside the carriageway they leave, in the same
/// direction, and would otherwise win a point here and there — each win a
/// sudden narrowing of the road for a hundred metres.
const LINK_PENALTY_M: f32 = 6.0;

/// Width of painted lines, in metres.
const LINE_HALF_WIDTH_M: f32 = 0.07;
/// A dashed lane line: painted this long in every period.
const DASH_ON_M: f32 = 3.0;
const DASH_PERIOD_M: f32 = 9.0;
/// Edge lines sit this far inside the edge of the tarmac.
const EDGE_INSET_M: f32 = 0.3;

/// Lift of paint above the road, and of other roads above the ground, in
/// metres. Other roads sit on the ground just under the ridden road, so where
/// they meet it the ridden road is the one seen.
const PAINT_LIFT_M: f32 = 0.02;
const OTHER_ROAD_LIFT_M: f32 = 0.06;

/// Other roads are laid on the ground in pieces no longer than this, so they
/// follow its shape.
const DRAPE_STEP_M: f32 = 10.0;

/// Two points of the route this far apart along it are different passes over
/// the same ground — a loop, an interchange — not one stretch of road.
pub const SAME_PASS_M: f32 = 150.0;
/// A pass of the route this much higher than another one beneath it is a
/// bridge over it.
const OVER_SELF_M: f32 = 3.0;

/// Where a road crosses over the ridden one, the top of its deck stands this
/// high above the ridden road — clearance for traffic and the deck itself.
const OVERPASS_M: f32 = 5.5;
/// Where a road passes under the ridden one, it sits this far below it.
const UNDERPASS_M: f32 = 6.0;
/// Grade of the ramps up to an overpass and down to an underpass: a real
/// flyover's 6 %.
const RAMP_GRADE: f32 = 0.06;
/// A road this far above the ground under it is drawn as a deck: with
/// thickness, and pillars where it stands high.
const DECK_FROM_M: f32 = 1.5;
const DECK_M: f32 = 1.0;
const PILLAR_EVERY_M: f32 = 30.0;
const PILLAR_HALF_M: f32 = 0.5;
const PILLAR_FROM_M: f32 = 3.0;
/// The ridden road's own bridges and tunnels.
const PARAPET_M: f32 = 1.0;
const BRIDGE_DECK_M: f32 = 1.2;
const TUNNEL_HEIGHT_M: f32 = 5.0;
const TUNNEL_SLAB_M: f32 = 0.6;
/// Walls stand this far outside the tarmac.
const WALL_GAP_M: f32 = 0.3;

const ASPHALT_RGB: [f32; 3] = [0.30, 0.31, 0.33];
const CONCRETE_RGB: [f32; 3] = [0.64, 0.63, 0.60];
const TUNNEL_RGB: [f32; 3] = [0.46, 0.45, 0.43];
const CYCLEWAY_RGB: [f32; 3] = [0.44, 0.33, 0.31];
const PAINT_RGB: [f32; 3] = [0.92, 0.92, 0.88];

/// Bucket width for the segment index, in metres.
const CELL_M: f32 = 100.0;

/// The cross-section of the road at one point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub lane_w: f32,
    /// Lanes running the rider's way.
    pub ours: u8,
    /// Lanes drawn: the rider's, and on a two-way road the oncoming ones.
    pub total: u8,
    pub oneway: bool,
    /// Solid lines along both edges — main roads have them.
    pub edge_lines: bool,
}

impl Profile {
    /// The road the view draws where the map says nothing: two lanes, one
    /// each way, the rider in the right-hand one.
    pub const DEFAULT: Profile = Profile {
        lane_w: 3.0,
        ours: 1,
        total: 2,
        oneway: false,
        edge_lines: false,
    };

    /// The cross-section of a mapped road.
    pub fn of(road: &Road) -> Self {
        if road.kind == RoadKind::Footway {
            return Self {
                lane_w: 3.0,
                ours: 1,
                total: 1,
                oneway: true,
                edge_lines: false,
            };
        }
        if road.kind == RoadKind::Cycleway {
            return Self {
                lane_w: 2.5,
                ours: 1,
                total: 1,
                oneway: true,
                edge_lines: false,
            };
        }
        let lane_w = match road.kind {
            RoadKind::Motorway | RoadKind::Trunk => 3.5,
            RoadKind::Primary | RoadKind::Secondary => 3.25,
            RoadKind::Service => 2.75,
            _ => 3.0,
        };
        let total = road
            .lanes
            .unwrap_or_else(|| default_lanes(road.kind, road.link, road.oneway))
            .clamp(1, 8);
        let ours = if road.oneway {
            total
        } else {
            total.div_ceil(2)
        };
        Self {
            lane_w,
            ours,
            total,
            oneway: road.oneway,
            edge_lines: !road.link
                && matches!(
                    road.kind,
                    RoadKind::Motorway | RoadKind::Trunk | RoadKind::Primary | RoadKind::Secondary
                ),
        }
    }

    /// Right edge of the tarmac, as an offset from the rider's line (positive
    /// right): half a lane, the rider being in the middle of the right lane.
    pub fn right(&self) -> f32 {
        self.lane_w / 2.0
    }

    /// Left edge of the tarmac.
    pub fn left(&self) -> f32 {
        self.right() - f32::from(self.total) * self.lane_w
    }

    /// How far the road reaches from the rider's line on its widest side.
    #[cfg(test)]
    pub fn extent(&self) -> f32 {
        self.right().abs().max(self.left().abs())
    }
}

/// Lanes a road has when the map does not say: total, both directions.
fn default_lanes(kind: RoadKind, link: bool, oneway: bool) -> u8 {
    if link {
        return 1;
    }
    match (kind, oneway) {
        (RoadKind::Motorway | RoadKind::Trunk, true) => 3,
        (RoadKind::Motorway | RoadKind::Trunk | RoadKind::Primary, false) => 4,
        (RoadKind::Primary | RoadKind::Secondary, true) => 2,
        (RoadKind::Service, false) => 1,
        (_, true) => 1,
        (_, false) => 2,
    }
}

/// A mapped road in scene space.
pub struct LocalRoad {
    pub id: u64,
    pub kind: RoadKind,
    pub link: bool,
    pub oneway: bool,
    pub level: Level,
    pub profile: Profile,
    pub points: Vec<(f32, f32)>,
}

/// Which level a road is on, and whether it is a bridge or in a tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Level {
    pub layer: i8,
    pub bridge: bool,
    pub tunnel: bool,
}

/// Place the map's roads in the scene.
pub fn localise(roads: &[Road], frame: &LocalFrame) -> Vec<LocalRoad> {
    roads
        .iter()
        .map(|r| LocalRoad {
            id: r.id,
            kind: r.kind,
            link: r.link,
            oneway: r.oneway,
            level: Level {
                layer: r.layer,
                bridge: r.bridge,
                tunnel: r.tunnel,
            },
            profile: Profile::of(r),
            points: r
                .points
                .iter()
                .map(|&(lat, lng)| {
                    let (e, n) = frame.to_local(lat, lng);
                    (e as f32, -n as f32)
                })
                .collect(),
        })
        .collect()
}

fn cell(x: f32, z: f32) -> (i64, i64) {
    ((x / CELL_M).floor() as i64, (z / CELL_M).floor() as i64)
}

/// Unit direction of travel at each point of the line, flat.
fn headings(line: &[RoadPoint]) -> Vec<(f32, f32)> {
    let n = line.len();
    (0..n)
        .map(|i| {
            let a = line[i.saturating_sub(2)].pos;
            let b = line[(i + 2).min(n - 1)].pos;
            let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
            let len = (dx * dx + dz * dz).sqrt();
            if len > 1e-3 {
                (dx / len, dz / len)
            } else {
                (0.0, -1.0)
            }
        })
        .collect()
}

/// Which mapped road the rider is on at each point of the line, if any —
/// the nearest within [`MATCH_M`] running their way, then pooled over the
/// points around it so a wobble does not switch roads.
pub fn match_roads(line: &[RoadPoint], roads: &[LocalRoad]) -> Vec<Option<usize>> {
    if line.is_empty() {
        return Vec::new();
    }
    let mut index: HashMap<(i64, i64), Vec<(usize, usize)>> = HashMap::new();
    // A footbridge is drawn, never ridden.
    for (r, road) in roads
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind != RoadKind::Footway)
    {
        for s in 0..road.points.len().saturating_sub(1) {
            let (a, b) = (road.points[s], road.points[s + 1]);
            let (lo, hi) = (
                cell(a.0.min(b.0), a.1.min(b.1)),
                cell(a.0.max(b.0), a.1.max(b.1)),
            );
            // A segment longer than a few buckets is filed in each it crosses.
            if (hi.0 - lo.0 + 1) * (hi.1 - lo.1 + 1) > 400 {
                continue;
            }
            for i in lo.0..=hi.0 {
                for j in lo.1..=hi.1 {
                    index.entry((i, j)).or_default().push((r, s));
                }
            }
        }
    }
    let cos_limit = MATCH_ANGLE_DEG.to_radians().cos();
    let heads = headings(line);
    let raw: Vec<Option<usize>> = line
        .iter()
        .zip(&heads)
        .map(|(p, &(hx, hz))| {
            let (x, z) = (p.pos[0], p.pos[2]);
            let (kx, kz) = cell(x, z);
            let mut best: Option<(f32, usize)> = None;
            for dx in -1..=1 {
                for dz in -1..=1 {
                    for &(r, s) in index.get(&(kx + dx, kz + dz)).into_iter().flatten() {
                        let road = &roads[r];
                        let (a, b) = (road.points[s], road.points[s + 1]);
                        let (sx, sz) = (b.0 - a.0, b.1 - a.1);
                        let len = (sx * sx + sz * sz).sqrt();
                        if len < 1e-3 {
                            continue;
                        }
                        let cos = (sx * hx + sz * hz) / len;
                        let aligned = if road.oneway { cos } else { cos.abs() };
                        if aligned < cos_limit {
                            continue;
                        }
                        let d = segment_distance(a, b, x, z);
                        if d > MATCH_M {
                            continue;
                        }
                        let score =
                            d + match road.kind {
                                RoadKind::Cycleway => CYCLEWAY_PENALTY_M,
                                RoadKind::Service => SERVICE_PENALTY_M,
                                _ => 0.0,
                            } + if road.link { LINK_PENALTY_M } else { 0.0 };
                        if best.is_none_or(|(b, _)| score < b) {
                            best = Some((score, r));
                        }
                    }
                }
            }
            best.map(|(_, r)| r)
        })
        .collect();
    // The most common *shape* of road around each point, "no road" included.
    // Voting by shape rather than by way: the map splits one boulevard into
    // many short ways, and split votes let a lone slip road win.
    let shape = |m: Option<usize>| {
        m.map(|r| {
            let p = roads[r].profile;
            (p.total, p.lane_w.to_bits(), p.oneway, p.edge_lines)
        })
    };
    (0..raw.len())
        .map(|i| {
            let lo = i.saturating_sub(MATCH_SMOOTH);
            let hi = (i + MATCH_SMOOTH).min(raw.len() - 1);
            let mut counts: HashMap<_, usize> = HashMap::new();
            for &m in &raw[lo..=hi] {
                *counts.entry(shape(m)).or_default() += 1;
            }
            // Ties go to this point's own answer, then to the narrower road,
            // so the result is the same every build.
            let own = shape(raw[i]);
            let winner = counts
                .into_iter()
                .max_by(|a, b| {
                    a.1.cmp(&b.1)
                        .then((a.0 == own).cmp(&(b.0 == own)))
                        .then(b.0.cmp(&a.0))
                })
                .map(|(k, _)| k)
                .unwrap_or(own);
            // This point's own road if it has the winning shape, else the
            // nearest one around it that does.
            if shape(raw[i]) == winner {
                return raw[i];
            }
            (1..=MATCH_SMOOTH)
                .flat_map(|k| [i.checked_sub(k), Some(i + k)])
                .flatten()
                .filter(|&j| j < raw.len())
                .map(|j| raw[j])
                .find(|&m| shape(m) == winner)
                .unwrap_or(raw[i])
        })
        .collect()
}

fn segment_distance(a: (f32, f32), b: (f32, f32), x: f32, z: f32) -> f32 {
    let (dx, dz) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dz * dz;
    let t = if len2 > 0.0 {
        (((x - a.0) * dx + (z - a.1) * dz) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((a.0 + t * dx - x).powi(2) + (a.1 + t * dz - z).powi(2)).sqrt()
}

/// The road ridden, point by point along the line.
pub struct Ridden {
    pub profiles: Vec<Profile>,
    /// The level of the road at each point: a bridge, a tunnel, or neither.
    pub levels: Vec<Level>,
    /// Smoothed left and right edges, offsets from the line.
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Ids of the mapped roads matched — drawn as the ridden road, so not
    /// again as other roads.
    pub matched: std::collections::HashSet<u64>,
}

impl Ridden {
    /// The road along `line`, from the mapped `roads` (or the default road
    /// throughout if there are none).
    pub fn new(line: &[RoadPoint], roads: &[LocalRoad]) -> Self {
        let matches = match_roads(line, roads);
        let profiles: Vec<Profile> = matches
            .iter()
            .map(|m| m.map_or(Profile::DEFAULT, |r| roads[r].profile))
            .collect();
        let matched = matches.iter().flatten().map(|&r| roads[r].id).collect();
        let mut levels: Vec<Level> = matches
            .iter()
            .map(|m| m.map_or(Level::default(), |r| roads[r].level))
            .collect();
        let smooth = |f: fn(&Profile) -> f32| -> Vec<f32> {
            let n = profiles.len();
            (0..n)
                .map(|i| {
                    let lo = i.saturating_sub(EDGE_SMOOTH);
                    let hi = (i + EDGE_SMOOTH).min(n - 1);
                    profiles[lo..=hi].iter().map(f).sum::<f32>() / (hi - lo + 1) as f32
                })
                .collect()
        };
        let left = smooth(Profile::left);
        let right = smooth(Profile::right);
        mark_passes_over_itself(line, &left, &right, &mut levels);
        Self {
            left,
            right,
            profiles,
            levels,
            matched,
        }
    }

    /// How far the road reaches from the line at each point, on its wider
    /// side — what the flat ground beside it must cover.
    pub fn extents(&self) -> Vec<f32> {
        self.left
            .iter()
            .zip(&self.right)
            .map(|(l, r)| l.abs().max(r.abs()))
            .collect()
    }
}

/// Where the route passes over itself — a loop, an interchange — the upper
/// pass is a bridge over the lower, whatever the map says: with a deck and
/// parapets, and no ground under it to bury the road below.
fn mark_passes_over_itself(line: &[RoadPoint], left: &[f32], right: &[f32], levels: &mut [Level]) {
    // Buckets as wide as the furthest reach, so one ring around a point's
    // bucket holds every point that can count.
    let cell = |x: f32, z: f32| ((x / 80.0).floor() as i64, (z / 80.0).floor() as i64);
    let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (j, p) in line.iter().enumerate() {
        buckets.entry(cell(p.pos[0], p.pos[2])).or_default().push(j);
    }
    for (i, p) in line.iter().enumerate() {
        // Wherever the ground beside this pass is held down for the pass
        // below — the road's reach and its shoulder — this pass stands clear
        // of the ground, so it is drawn as a bridge.
        let reach = left[i].abs().max(right[i].abs()) + crate::data::course3d::SHOULDER_BASE_M;
        let (kx, kz) = cell(p.pos[0], p.pos[2]);
        let over = (-1..=1).any(|dx| {
            (-1..=1).any(|dz| {
                buckets.get(&(kx + dx, kz + dz)).is_some_and(|js| {
                    js.iter().any(|&j| {
                        let q = &line[j];
                        (q.distance_m - p.distance_m).abs() > SAME_PASS_M
                            && q.pos[1] < p.pos[1] - OVER_SELF_M
                            && (q.pos[0] - p.pos[0]).hypot(q.pos[2] - p.pos[2]) <= reach
                    })
                })
            })
        });
        if over {
            levels[i].bridge = true;
            levels[i].layer = levels[i].layer.max(1);
        }
    }
}

/// Perpendicular to the line at each point, pointing right.
fn rights(line: &[RoadPoint]) -> Vec<(f32, f32)> {
    headings(line)
        .into_iter()
        .map(|(hx, hz)| (-hz, hx))
        .collect()
}

/// A strip along the line between per-point offsets `from` and `to` (right
/// positive), at `lift` above the line, drawn where `keep(i)` holds for the
/// segment starting at point `i`.
fn strip(
    line: &[RoadPoint],
    from: impl Fn(usize) -> f32,
    to: impl Fn(usize) -> f32,
    lift: f32,
    colour: [f32; 4],
    keep: impl Fn(usize) -> bool,
) -> Mesh {
    let right = rights(line);
    let pair = |i: usize| {
        let (p, (rx, rz)) = (line[i].pos, right[i]);
        let y = p[1] + lift;
        let at = |off: f32| [p[0] + rx * off, y, p[2] + rz * off];
        [at(from(i)), at(to(i))]
    };
    quads_along(line.len(), pair, colour, keep)
}

/// Quads joining pairs of points along a line — each point gives two corners,
/// each kept segment one quad. Vertices are made only for kept segments, and
/// shared between neighbours in a run: a structure drawn on a few hundred
/// metres of a 40 km route costs a few hundred metres of vertices.
fn quads_along(
    points: usize,
    pair: impl Fn(usize) -> [[f32; 3]; 2],
    colour: [f32; 4],
    keep: impl Fn(usize) -> bool,
) -> Mesh {
    let mut mesh = Mesh::default();
    let mut last: Option<(usize, u32)> = None;
    let push = |mesh: &mut Mesh, i: usize| -> u32 {
        let base = mesh.vertices.len() as u32;
        for pos in pair(i) {
            mesh.vertices.push(Vertex {
                pos,
                colour,
                uv: NO_UV,
            });
        }
        base
    };
    for i in 0..points.saturating_sub(1) {
        if !keep(i) {
            continue;
        }
        let a = match last {
            Some((k, base)) if k == i => base,
            _ => push(&mut mesh, i),
        };
        let b = push(&mut mesh, i + 1);
        mesh.indices
            .extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        last = Some((i + 1, b));
    }
    mesh
}

/// Dashes along the line at `offset(i)` (right positive), each [`DASH_ON_M`]
/// long every [`DASH_PERIOD_M`], placed by distance along the route and cut
/// to exact length — the GPX's own points are 8–9 m apart, as long as the
/// pattern, so dashes cut from its segments ran together into a solid line.
/// A dash is drawn only where `has(i)` holds at both its ends.
fn dashes(
    line: &[RoadPoint],
    offset: impl Fn(usize) -> f32,
    has: impl Fn(usize) -> bool,
    colour: [f32; 4],
) -> Mesh {
    let mut mesh = Mesh::default();
    let (Some(first), Some(last)) = (line.first(), line.last()) else {
        return mesh;
    };
    if line.len() < 2 {
        return mesh;
    }
    let right = rights(line);
    // Point, right vector and offset at a distance along the line, with the
    // index of the segment it falls in.
    let at = |d: f32| {
        let i = line
            .partition_point(|p| p.distance_m <= d)
            .clamp(1, line.len() - 1);
        let (a, b) = (&line[i - 1], &line[i]);
        let span = b.distance_m - a.distance_m;
        let t = if span > 1e-3 {
            ((d - a.distance_m) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mix = |u: f32, v: f32| u + (v - u) * t;
        let pos = [
            mix(a.pos[0], b.pos[0]),
            mix(a.pos[1], b.pos[1]),
            mix(a.pos[2], b.pos[2]),
        ];
        let (ra, rb) = (right[i - 1], right[i]);
        let r = (mix(ra.0, rb.0), mix(ra.1, rb.1));
        (pos, r, mix(offset(i - 1), offset(i)), i)
    };
    let mut d = (first.distance_m / DASH_PERIOD_M).ceil() * DASH_PERIOD_M;
    while d + DASH_ON_M <= last.distance_m {
        let (p0, r0, o0, i0) = at(d);
        let (p1, r1, o1, i1) = at(d + DASH_ON_M);
        if has(i0 - 1) && has(i0) && has(i1 - 1) && has(i1) {
            let v = mesh.vertices.len() as u32;
            for (p, r, o) in [(p0, r0, o0), (p1, r1, o1)] {
                for side in [-LINE_HALF_WIDTH_M, LINE_HALF_WIDTH_M] {
                    let off = o + side;
                    mesh.vertices.push(Vertex {
                        pos: [p[0] + r.0 * off, p[1] + PAINT_LIFT_M, p[2] + r.1 * off],
                        colour,
                        uv: NO_UV,
                    });
                }
            }
            mesh.indices
                .extend_from_slice(&[v, v + 1, v + 2, v + 1, v + 3, v + 2]);
        }
        d += DASH_PERIOD_M;
    }
    mesh
}

/// The ridden road's tarmac and paint.
pub fn ridden_mesh(line: &[RoadPoint], ridden: &Ridden) -> Mesh {
    let asphalt = [
        ASPHALT_RGB[0],
        ASPHALT_RGB[1],
        ASPHALT_RGB[2],
        MATERIAL_ROAD,
    ];
    let paint = [PAINT_RGB[0], PAINT_RGB[1], PAINT_RGB[2], MATERIAL_PLAIN];
    let mut mesh = strip(
        line,
        |i| ridden.left[i],
        |i| ridden.right[i],
        0.0,
        asphalt,
        |_| true,
    );
    let profile = |i: usize| ridden.profiles[i];
    // Lines between lanes, counted from the right edge: line k sits k lanes
    // in. Drawn on a segment only where both its ends have that line, so a
    // lane appearing or ending stops its line rather than bending it.
    let max_lines = ridden.profiles.iter().map(|p| p.total).max().unwrap_or(1);
    for k in 1..max_lines {
        let at = |i: usize| ridden.right[i] - f32::from(k) * profile(i).lane_w;
        // Only where the line lies on the tarmac: where the road is widening
        // or narrowing, its smoothed edge is inside where the lanes say.
        mesh.extend(dashes(
            line,
            at,
            |i| k < profile(i).total && at(i) > ridden.left[i] + EDGE_INSET_M,
            paint,
        ));
    }
    // Edge lines on main roads.
    let edged = |i: usize| profile(i).edge_lines && profile(i + 1).edge_lines;
    for (inner, edge) in [
        (-EDGE_INSET_M, &ridden.right as &Vec<f32>),
        (EDGE_INSET_M, &ridden.left as &Vec<f32>),
    ] {
        mesh.extend(strip(
            line,
            |i| edge[i] + inner - LINE_HALF_WIDTH_M,
            |i| edge[i] + inner + LINE_HALF_WIDTH_M,
            PAINT_LIFT_M,
            paint,
            edged,
        ));
    }
    mesh
}

/// A vertical face along the line at `offset(i)`, from height `bottom(i)` to
/// `top(i)` (absolute), drawn where `keep(i)` holds for the segment from `i`.
fn wall(
    line: &[RoadPoint],
    offset: impl Fn(usize) -> f32,
    bottom: impl Fn(usize) -> f32,
    top: impl Fn(usize) -> f32,
    colour: [f32; 4],
    keep: impl Fn(usize) -> bool,
) -> Mesh {
    let right = rights(line);
    let pair = |i: usize| {
        let (p, (rx, rz)) = (line[i].pos, right[i]);
        let o = offset(i);
        let (x, z) = (p[0] + rx * o, p[2] + rz * o);
        [[x, bottom(i), z], [x, top(i), z]]
    };
    quads_along(line.len(), pair, colour, keep)
}

/// The ridden road's own structures: parapets and a deck edge where it is a
/// bridge, walls and a ceiling where it is in a tunnel.
///
/// `ground(x, z)` is the drawn ground's height, which bridge pillars stand on.
pub fn ridden_structures(
    line: &[RoadPoint],
    ridden: &Ridden,
    ground: impl Fn(f32, f32) -> Option<f32>,
) -> Mesh {
    let concrete = [
        CONCRETE_RGB[0],
        CONCRETE_RGB[1],
        CONCRETE_RGB[2],
        MATERIAL_PLAIN,
    ];
    let tunnel_rgb = [TUNNEL_RGB[0], TUNNEL_RGB[1], TUNNEL_RGB[2], MATERIAL_PLAIN];
    let y = |i: usize| line[i].pos[1];
    let bridge = |i: usize| ridden.levels[i].bridge && ridden.levels[i + 1].bridge;
    let tunnel = |i: usize| ridden.levels[i].tunnel && ridden.levels[i + 1].tunnel;
    let mut mesh = Mesh::default();
    for edge in [&ridden.right, &ridden.left] {
        let outward = if std::ptr::eq(edge, &ridden.right) {
            WALL_GAP_M
        } else {
            -WALL_GAP_M
        };
        // Parapet above the deck, and the deck's own edge below it.
        mesh.extend(wall(
            line,
            |i| edge[i] + outward,
            y,
            |i| y(i) + PARAPET_M,
            concrete,
            bridge,
        ));
        mesh.extend(wall(
            line,
            |i| edge[i],
            |i| y(i) - BRIDGE_DECK_M,
            y,
            concrete,
            bridge,
        ));
        // Tunnel walls up to the ceiling.
        mesh.extend(wall(
            line,
            |i| edge[i] + outward,
            y,
            |i| y(i) + TUNNEL_HEIGHT_M,
            tunnel_rgb,
            tunnel,
        ));
    }
    // Pillars under the bridge, down to the ground, wherever it stands clear.
    let mut next_pillar = f32::MIN;
    for (i, p) in line.iter().enumerate() {
        if !ridden.levels[i].bridge || p.distance_m < next_pillar {
            continue;
        }
        // Under the middle of the tarmac.
        let mid = (ridden.left[i] + ridden.right[i]) / 2.0;
        let (rx, rz) = rights(&line[i.saturating_sub(1)..(i + 2).min(line.len())])
            .get(usize::from(i > 0))
            .copied()
            .unwrap_or((1.0, 0.0));
        let (x, z) = (p.pos[0] + rx * mid, p.pos[2] + rz * mid);
        let underside = p.pos[1] - BRIDGE_DECK_M;
        if let Some(g) = ground(x, z).filter(|g| underside - g > PILLAR_FROM_M - DECK_M) {
            mesh.extend(pillar(x, z, g, underside, concrete));
            next_pillar = p.distance_m + PILLAR_EVERY_M;
        }
    }
    for lift in [TUNNEL_HEIGHT_M, TUNNEL_HEIGHT_M + TUNNEL_SLAB_M] {
        mesh.extend(strip(
            line,
            |i| ridden.left[i] - WALL_GAP_M,
            |i| ridden.right[i] + WALL_GAP_M,
            lift,
            tunnel_rgb,
            tunnel,
        ));
    }
    mesh
}

/// Where another road crosses the ridden one: how far along the other road,
/// how high the ridden road is there, and its level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    pub along_m: f32,
    pub y: f32,
    pub layer: i8,
}

/// Every place `points` crosses the ridden `line`, in order along `points`.
pub fn crossings(points: &[(f32, f32)], line: &[RoadPoint], levels: &[Level]) -> Vec<Crossing> {
    let mut out = Vec::new();
    let mut along = 0.0f32;
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        for i in 0..line.len().saturating_sub(1) {
            let (p, q) = (line[i].pos, line[i + 1].pos);
            if let Some((t, u)) = intersect(a, b, (p[0], p[2]), (q[0], q[2])) {
                out.push(Crossing {
                    along_m: along + t * seg,
                    y: p[1] + (q[1] - p[1]) * u,
                    layer: levels.get(i).map_or(0, |l| l.layer),
                });
            }
        }
        along += seg;
    }
    out.sort_by(|x, y| x.along_m.total_cmp(&y.along_m));
    out
}

/// Where segments `a–b` and `c–d` cross, as the fractions along each.
fn intersect(a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32)) -> Option<(f32, f32)> {
    let r = (b.0 - a.0, b.1 - a.1);
    let s = (d.0 - c.0, d.1 - c.1);
    let denom = r.0 * s.1 - r.1 * s.0;
    if denom.abs() < 1e-9 {
        return None; // parallel
    }
    let qp = (c.0 - a.0, c.1 - a.1);
    let t = (qp.0 * s.1 - qp.1 * s.0) / denom;
    let u = (qp.0 * r.1 - qp.1 * r.0) / denom;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then_some((t, u))
}

/// How high another road runs at `along_m` metres along it, given the ground
/// there and where it crosses the ridden road.
///
/// It lies on the ground, except: over the ridden road (a higher layer) it
/// rises on a ramp to [`OVERPASS_M`] above it; under it (a lower layer) it
/// dips on a ramp to [`UNDERPASS_M`] below; and a bridge never sags below the
/// straight line between its two ends, `ends` — it spans what it crosses.
pub fn deck_height(
    ground: f32,
    along_m: f32,
    layer: i8,
    crossings: &[Crossing],
    ends: Option<(f32, f32, f32)>,
) -> f32 {
    let mut h = ground;
    if let Some((start, end, length)) = ends {
        let t = if length > 0.0 {
            (along_m / length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        h = h.max(start + (end - start) * t);
    }
    for c in crossings {
        let ramp = RAMP_GRADE * (along_m - c.along_m).abs();
        if layer > c.layer {
            h = h.max(c.y + OVERPASS_M - ramp);
        }
    }
    for c in crossings {
        let ramp = RAMP_GRADE * (along_m - c.along_m).abs();
        if layer < c.layer {
            h = h.min(c.y - UNDERPASS_M + ramp);
        }
    }
    h
}

/// Every mapped road except the ones ridden — side streets, the opposite
/// carriageway, cycle paths, footbridges — laid on the ground, or carried over
/// and under the ridden road as their levels say. `ground(x, z)` is the drawn
/// ground's height; a road is drawn only where there is ground under it.
/// Tunnels are left out: they run under the ground drawn above them.
pub fn other_roads_mesh(
    roads: &[LocalRoad],
    line: &[RoadPoint],
    ridden: &Ridden,
    ground: impl Fn(f32, f32) -> Option<f32>,
) -> Mesh {
    let concrete = [
        CONCRETE_RGB[0],
        CONCRETE_RGB[1],
        CONCRETE_RGB[2],
        MATERIAL_PLAIN,
    ];
    let mut mesh = Mesh::default();
    for road in roads
        .iter()
        .filter(|r| !ridden.matched.contains(&r.id) && !r.level.tunnel)
    {
        let rgb = match road.kind {
            RoadKind::Cycleway => CYCLEWAY_RGB,
            RoadKind::Footway => CONCRETE_RGB,
            _ => ASPHALT_RGB,
        };
        let colour = [rgb[0], rgb[1], rgb[2], MATERIAL_ROAD];
        let half = f32::from(road.profile.total) * road.profile.lane_w / 2.0;
        let crossed = crossings(&road.points, line, &ridden.levels);
        // Densify so the strip follows the ground, keeping distance along.
        let mut pts: Vec<(f32, f32, f32)> = Vec::new();
        let mut along = 0.0f32;
        for w in road.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            let steps = (len / DRAPE_STEP_M).ceil().max(1.0) as usize;
            for k in 0..steps {
                let t = k as f32 / steps as f32;
                pts.push((
                    a.0 + (b.0 - a.0) * t,
                    a.1 + (b.1 - a.1) * t,
                    along + len * t,
                ));
            }
            along += len;
        }
        if let Some(&last) = road.points.last() {
            pts.push((last.0, last.1, along));
        }
        // A bridge spans between the ground at its two ends.
        let ends = if road.level.bridge {
            match (pts.first(), pts.last()) {
                (Some(&(x0, z0, _)), Some(&(x1, z1, l))) => ground(x0, z0)
                    .zip(ground(x1, z1))
                    .map(|(g0, g1)| (g0, g1, l)),
                _ => None,
            }
        } else {
            None
        };
        // Each run of points with ground under them becomes one strip; where
        // the road stands clear of the ground it gets a deck's thickness and
        // pillars.
        let mut run: Vec<(RoadPoint, f32)> = Vec::new();
        let flush = |run: &mut Vec<(RoadPoint, f32)>, mesh: &mut Mesh| {
            if run.len() >= 2 {
                let line: Vec<RoadPoint> = run.iter().map(|(p, _)| *p).collect();
                let raised = |i: usize| run[i].0.pos[1] - run[i].1 > DECK_FROM_M;
                mesh.extend(strip(&line, |_| -half, |_| half, 0.0, colour, |_| true));
                // Most roads never leave the ground; only those that do get a
                // deck's sides, underside and pillars.
                if !(0..run.len()).any(raised) {
                    run.clear();
                    return;
                }
                for side in [-half, half] {
                    mesh.extend(wall(
                        &line,
                        |_| side,
                        |i| line[i].pos[1] - DECK_M,
                        |i| line[i].pos[1],
                        concrete,
                        |i| raised(i) || raised(i + 1),
                    ));
                }
                mesh.extend(strip(
                    &line,
                    |_| -half,
                    |_| half,
                    -DECK_M,
                    concrete,
                    |i| raised(i) && raised(i + 1),
                ));
                let mut next_pillar = 0.0f32;
                for (p, g) in run.iter() {
                    if p.distance_m >= next_pillar && p.pos[1] - g > PILLAR_FROM_M {
                        mesh.extend(pillar(p.pos[0], p.pos[2], *g, p.pos[1] - DECK_M, concrete));
                        next_pillar = p.distance_m + PILLAR_EVERY_M;
                    }
                }
            }
            run.clear();
        };
        for &(x, z, s_along) in &pts {
            match ground(x, z) {
                Some(g) => {
                    let h = deck_height(g, s_along, road.level.layer, &crossed, ends);
                    run.push((
                        RoadPoint {
                            pos: [x, h + OTHER_ROAD_LIFT_M, z],
                            distance_m: s_along,
                        },
                        g,
                    ));
                }
                None => flush(&mut run, &mut mesh),
            }
        }
        flush(&mut run, &mut mesh);
    }
    mesh
}

/// A square pillar from `bottom` to `top`, centred on `(x, z)`.
fn pillar(x: f32, z: f32, bottom: f32, top: f32, colour: [f32; 4]) -> Mesh {
    let h = PILLAR_HALF_M;
    let corners = [
        (x - h, z - h),
        (x + h, z - h),
        (x + h, z + h),
        (x - h, z + h),
    ];
    let mut mesh = Mesh::default();
    for k in 0..4 {
        let (a, b) = (corners[k], corners[(k + 1) % 4]);
        let v = mesh.vertices.len() as u32;
        for pos in [
            [a.0, bottom, a.1],
            [b.0, bottom, b.1],
            [b.0, top, b.1],
            [a.0, top, a.1],
        ] {
            mesh.vertices.push(Vertex {
                pos,
                colour,
                uv: NO_UV,
            });
        }
        mesh.indices
            .extend_from_slice(&[v, v + 1, v + 2, v, v + 2, v + 3]);
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::osm::Road;

    fn road(kind: RoadKind, lanes: Option<u8>, oneway: bool) -> Road {
        Road {
            id: 1,
            kind,
            link: false,
            lanes,
            oneway,
            layer: 0,
            bridge: false,
            tunnel: false,
            points: Vec::new(),
        }
    }

    // ── Cross-sections ────────────────────────────────────────────────────────

    #[test]
    fn should_put_the_rider_in_the_middle_of_the_right_lane() {
        // The coastal boulevard: one carriageway of three 3.5 m lanes.
        let p = Profile::of(&road(RoadKind::Trunk, Some(3), true));
        assert_eq!(p.right(), 1.75);
        assert_eq!(p.left(), 1.75 - 3.0 * 3.5);
        assert_eq!((p.ours, p.total), (3, 3));
        assert!(p.edge_lines);
    }

    #[test]
    fn should_draw_both_directions_of_a_two_way_road() {
        let p = Profile::of(&road(RoadKind::Primary, Some(4), false));
        assert_eq!((p.ours, p.total), (2, 4));
        assert_eq!(p.left(), p.right() - 4.0 * 3.25);
        // An odd count gives the rider's side the extra lane.
        let p = Profile::of(&road(RoadKind::Secondary, Some(3), false));
        assert_eq!(p.ours, 2);
    }

    #[test]
    fn should_agree_with_the_ground_on_how_far_the_default_road_reaches() {
        // The flat shoulder beside an unmatched road is sized from this.
        assert_eq!(
            Profile::DEFAULT.extent(),
            crate::data::course3d::DEFAULT_EXTENT_M
        );
    }

    #[test]
    fn should_keep_todays_road_where_the_map_says_nothing() {
        let p = Profile::DEFAULT;
        assert_eq!(p.right(), 1.5);
        assert_eq!(p.left(), -4.5);
        // A residential street with no lanes tag is the same road.
        assert_eq!(Profile::of(&road(RoadKind::Minor, None, false)), p);
    }

    #[test]
    fn should_guess_a_sensible_number_of_lanes() {
        assert_eq!(Profile::of(&road(RoadKind::Trunk, None, true)).total, 3);
        assert_eq!(Profile::of(&road(RoadKind::Primary, None, false)).total, 4);
        assert_eq!(Profile::of(&road(RoadKind::Service, None, false)).total, 1);
        let link = Road {
            link: true,
            ..road(RoadKind::Trunk, None, true)
        };
        assert_eq!(Profile::of(&link).total, 1);
        assert!(!Profile::of(&link).edge_lines);
    }

    #[test]
    fn should_hold_a_mapped_lane_count_to_what_can_be_drawn() {
        assert_eq!(
            Profile::of(&road(RoadKind::Motorway, Some(16), true)).total,
            8
        );
    }

    #[test]
    fn should_ride_a_cycleway_as_one_narrow_lane() {
        let p = Profile::of(&road(RoadKind::Cycleway, Some(2), false));
        assert_eq!((p.total, p.lane_w), (1, 2.5));
        assert_eq!(p.extent(), 1.25);
    }

    // ── Matching ──────────────────────────────────────────────────────────────

    /// A GPX line due north (−z) along x = 0, a point every 8 m.
    fn north_line(points: usize) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 8.0],
                distance_m: i as f32 * 8.0,
            })
            .collect()
    }

    fn local(id: u64, kind: RoadKind, lanes: u8, oneway: bool, pts: Vec<(f32, f32)>) -> LocalRoad {
        let r = road(kind, Some(lanes), oneway);
        LocalRoad {
            id,
            kind,
            link: false,
            oneway,
            level: Level::default(),
            profile: Profile::of(&r),
            points: pts,
        }
    }

    #[test]
    fn should_match_the_carriageway_under_the_route_not_the_opposite_one() {
        let line = north_line(50);
        let roads = vec![
            // Our carriageway, 4 m off the GPS line, running north.
            local(
                10,
                RoadKind::Trunk,
                3,
                true,
                vec![(-4.0, 100.0), (-4.0, -600.0)],
            ),
            // The opposite carriageway, 2 m away — nearer — but running south.
            local(
                11,
                RoadKind::Trunk,
                3,
                true,
                vec![(-2.0, -600.0), (-2.0, 100.0)],
            ),
        ];
        let m = match_roads(&line, &roads);
        assert!(m.iter().all(|&r| r == Some(0)), "{m:?}");
    }

    #[test]
    fn should_prefer_the_road_to_a_nearer_cycle_path() {
        let line = north_line(50);
        let roads = vec![
            local(
                1,
                RoadKind::Cycleway,
                1,
                false,
                vec![(2.0, 100.0), (2.0, -600.0)],
            ),
            local(
                2,
                RoadKind::Trunk,
                3,
                true,
                vec![(-6.0, 100.0), (-6.0, -600.0)],
            ),
        ];
        assert!(match_roads(&line, &roads).iter().all(|&r| r == Some(1)));
    }

    #[test]
    fn should_still_ride_a_cycle_path_when_there_is_no_road() {
        let line = north_line(50);
        let roads = vec![local(
            1,
            RoadKind::Cycleway,
            1,
            false,
            vec![(2.0, 100.0), (2.0, -600.0)],
        )];
        assert!(match_roads(&line, &roads).iter().all(|&r| r == Some(0)));
    }

    #[test]
    fn should_not_match_a_cross_street_or_a_far_road() {
        let line = north_line(50);
        let roads = vec![
            local(
                1,
                RoadKind::Minor,
                2,
                false,
                vec![(-300.0, -200.0), (300.0, -200.0)],
            ),
            local(
                2,
                RoadKind::Trunk,
                3,
                true,
                vec![(40.0, 100.0), (40.0, -600.0)],
            ),
        ];
        assert!(match_roads(&line, &roads).iter().all(Option::is_none));
    }

    #[test]
    fn should_not_switch_roads_for_one_wobble() {
        let mut line = north_line(50);
        // One point strays 30 m east, onto a parallel road there.
        line[25].pos[0] = 30.0;
        let roads = vec![
            local(
                1,
                RoadKind::Trunk,
                3,
                true,
                vec![(0.0, 100.0), (0.0, -600.0)],
            ),
            local(
                2,
                RoadKind::Primary,
                2,
                true,
                vec![(30.0, 100.0), (30.0, -600.0)],
            ),
        ];
        let m = match_roads(&line, &roads);
        assert_eq!(m[25], Some(0), "{m:?}");
    }

    #[test]
    fn should_match_nothing_with_no_roads_or_no_line() {
        assert!(match_roads(&north_line(10), &[])
            .iter()
            .all(Option::is_none));
        assert!(match_roads(&[], &[]).is_empty());
    }

    // ── The ridden road ───────────────────────────────────────────────────────

    #[test]
    fn should_widen_gradually_where_the_road_gains_lanes() {
        let line = north_line(100);
        // A three-lane carriageway for the first half only.
        let roads = vec![local(
            1,
            RoadKind::Trunk,
            3,
            true,
            vec![(0.0, 8.0), (0.0, -400.0)],
        )];
        let r = Ridden::new(&line, &roads);
        assert!(
            (r.left[10] - Profile::of(&road(RoadKind::Trunk, Some(3), true)).left()).abs() < 1e-3
        );
        assert!((r.left[90] - Profile::DEFAULT.left()).abs() < 1e-3);
        for w in r.left.windows(2) {
            assert!((w[1] - w[0]).abs() <= 1.0, "a step of {} m", w[1] - w[0]);
        }
        assert!(r.matched.contains(&1));
    }

    #[test]
    fn should_report_how_far_the_road_reaches() {
        let r = Ridden::new(&north_line(20), &[]);
        assert!(r.extents().iter().all(|&e| e == 4.5));
    }

    #[test]
    fn should_cut_dashes_to_length_whatever_the_gps_spacing() {
        // Points 8.5 m apart — as recorded — must still give 3 m dashes every 9 m.
        let line: Vec<RoadPoint> = (0..120)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 8.5],
                distance_m: i as f32 * 8.5,
            })
            .collect();
        let mesh = dashes(&line, |_| -1.5, |_| true, [1.0; 4]);
        let quads = mesh.indices.len() / 6;
        // 0 → 1011.5 m: dashes start at 0, 9, … up to 1008 (ending 1011).
        assert_eq!(quads, 113);
        for q in 0..quads {
            let v = &mesh.vertices[q * 4..q * 4 + 4];
            let len = (v[2].pos[2] - v[0].pos[2]).abs();
            assert!((len - DASH_ON_M).abs() < 1e-3, "dash {q} is {len} m");
            assert!((v[0].pos[0] - (-1.5 - LINE_HALF_WIDTH_M)).abs() < 1e-4);
        }
    }

    #[test]
    fn should_stop_a_lane_line_where_its_lane_ends() {
        let line = north_line(100);
        // The lane line exists only for the first 40 points.
        let mesh = dashes(&line, |_| -1.5, |i| i < 40, [1.0; 4]);
        assert!(mesh.vertices.iter().all(|v| -v.pos[2] <= 39.0 * 8.0 + 1e-3));
        assert!(!mesh.vertices.is_empty());
    }

    #[test]
    fn should_paint_three_lanes_with_two_dashed_lines_and_two_edges() {
        let line = north_line(200);
        let roads = vec![local(
            1,
            RoadKind::Trunk,
            3,
            true,
            vec![(0.0, 100.0), (0.0, -2_000.0)],
        )];
        let r = Ridden::new(&line, &roads);
        let mesh = ridden_mesh(&line, &r);
        // Paint x positions (the line runs north, so x is the offset).
        let mut xs: Vec<i32> = mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] == MATERIAL_PLAIN && v.pos[1] > 10.0)
            .map(|v| (v.pos[0] * 10.0).round() as i32)
            .collect();
        xs.sort_unstable();
        xs.dedup();
        // Lines at −1.75, −5.25 (dashes) and 1.45, −8.45 (edges), ± half width.
        for centre in [-1.75f32, -5.25, 1.45, -8.45] {
            assert!(
                xs.iter()
                    .any(|&x| ((x as f32) / 10.0 - centre).abs() < 0.11),
                "no line at {centre}: {xs:?}"
            );
        }
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
    }

    // ── Other roads ───────────────────────────────────────────────────────────

    #[test]
    fn should_lay_other_roads_on_the_ground_and_skip_the_ridden_one() {
        let roads = vec![
            local(1, RoadKind::Minor, 2, false, vec![(0.0, 0.0), (100.0, 0.0)]),
            local(
                2,
                RoadKind::Trunk,
                3,
                true,
                vec![(0.0, 50.0), (100.0, 50.0)],
            ),
        ];
        let ridden = Ridden {
            matched: [2u64].into_iter().collect(),
            ..Ridden::new(&[], &[])
        };
        let mesh = other_roads_mesh(&roads, &[], &ridden, |x, _| Some(x * 0.1));
        assert!(!mesh.vertices.is_empty());
        for v in &mesh.vertices {
            assert!(
                v.pos[2].abs() < 5.0,
                "the ridden road was drawn again: {:?}",
                v.pos
            );
            assert!((v.pos[1] - (v.pos[0].round() * 0.1 + OTHER_ROAD_LIFT_M)).abs() < 0.2);
        }
    }

    #[test]
    fn should_break_a_road_where_the_ground_runs_out() {
        let roads = vec![local(
            1,
            RoadKind::Minor,
            2,
            false,
            vec![(0.0, 0.0), (100.0, 0.0)],
        )];
        let gap = |x: f32, _z: f32| (!(40.0..60.0).contains(&x)).then_some(0.0);
        let mesh = other_roads_mesh(&roads, &[], &Ridden::new(&[], &[]), gap);
        // No vertex inside the gap, and no triangle spanning it.
        assert!(mesh
            .vertices
            .iter()
            .all(|v| !(41.0..59.0).contains(&v.pos[0])));
        for t in mesh.indices.chunks(3) {
            let xs: Vec<f32> = t
                .iter()
                .map(|&i| mesh.vertices[i as usize].pos[0])
                .collect();
            let span = xs.iter().cloned().fold(f32::MIN, f32::max)
                - xs.iter().cloned().fold(f32::MAX, f32::min);
            assert!(span < 15.0, "a triangle spans the gap: {xs:?}");
        }
    }

    // ── Over and under ────────────────────────────────────────────────────────

    #[test]
    fn should_find_where_a_cross_street_crosses_the_route() {
        let line = north_line(50); // x = 0, z from 0 to −392
        let levels = vec![Level::default(); 50];
        let c = crossings(&[(-50.0, -100.0), (50.0, -100.0)], &line, &levels);
        assert_eq!(c.len(), 1);
        assert!((c[0].along_m - 50.0).abs() < 1e-3, "{c:?}");
        assert_eq!(c[0].y, 10.0);
        assert!(crossings(&[(-50.0, 100.0), (50.0, 100.0)], &line, &levels).is_empty());
    }

    #[test]
    fn should_carry_a_higher_road_clear_over_the_route() {
        let over = [Crossing {
            along_m: 100.0,
            y: 10.0,
            layer: 0,
        }];
        // At the crossing: exactly the overpass height above the ridden road.
        assert_eq!(deck_height(9.7, 100.0, 1, &over, None), 10.0 + OVERPASS_M);
        // Down a 6 % ramp either side, to the ground.
        assert!((deck_height(9.7, 150.0, 1, &over, None) - (15.5 - 3.0)).abs() < 1e-4);
        assert_eq!(deck_height(9.7, 400.0, 1, &over, None), 9.7);
        // The same road at the same level as the route meets it at grade.
        assert_eq!(deck_height(9.7, 100.0, 0, &over, None), 9.7);
    }

    #[test]
    fn should_leave_room_for_the_rider_under_an_overpass() {
        // The deck's underside clears the rider's head with room to spare.
        let underside = OVERPASS_M - DECK_M;
        assert!(
            underside > crate::data::course3d::EYE_HEIGHT_M + 2.5,
            "{underside}"
        );
    }

    #[test]
    fn should_dip_a_lower_road_under_a_bridge_on_the_route() {
        let under = [Crossing {
            along_m: 50.0,
            y: 20.0,
            layer: 1,
        }];
        assert_eq!(deck_height(19.7, 50.0, 0, &under, None), 20.0 - UNDERPASS_M);
        // Never lifted above its ground by the dip rule.
        assert_eq!(deck_height(5.0, 50.0, 0, &under, None), 5.0);
    }

    #[test]
    fn should_span_a_bridge_straight_between_its_ends() {
        // A 200 m bridge from ground at 30 m to ground at 40 m, over a valley at 5 m.
        let mid = deck_height(5.0, 100.0, 1, &[], Some((30.0, 40.0, 200.0)));
        assert!((mid - 35.0).abs() < 1e-4, "{mid}");
        // Not a bridge: lies in the valley.
        assert_eq!(deck_height(5.0, 100.0, 0, &[], None), 5.0);
    }

    #[test]
    fn should_raise_an_overpass_over_the_route_and_hold_it_up() {
        let line = north_line(60);
        let ridden = Ridden::new(&line, &[]);
        let bridge = LocalRoad {
            level: Level {
                layer: 1,
                bridge: true,
                tunnel: false,
            },
            ..local(
                9,
                RoadKind::Primary,
                2,
                false,
                vec![(-200.0, -200.0), (200.0, -200.0)],
            )
        };
        let mesh = other_roads_mesh(&[bridge], &line, &ridden, |_, _| Some(9.7));
        let top = mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] == MATERIAL_ROAD)
            .map(|v| v.pos[1])
            .fold(f32::MIN, f32::max);
        assert!(
            (top - (10.0 + OVERPASS_M + OTHER_ROAD_LIFT_M)).abs() < 0.05,
            "{top}"
        );
        // Concrete: the deck's sides and at least one pillar under it.
        assert!(mesh
            .vertices
            .iter()
            .any(|v| v.colour[3] == MATERIAL_PLAIN && v.pos[1] < 9.8));
    }

    #[test]
    fn should_not_draw_a_tunnel_over_the_ground() {
        let tunnel = LocalRoad {
            level: Level {
                layer: -1,
                bridge: false,
                tunnel: true,
            },
            ..local(9, RoadKind::Trunk, 2, true, vec![(0.0, 0.0), (100.0, 0.0)])
        };
        let mesh = other_roads_mesh(&[tunnel], &[], &Ridden::new(&[], &[]), |_, _| Some(0.0));
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn should_never_ride_a_footbridge() {
        let line = north_line(50);
        let roads = vec![local(
            1,
            RoadKind::Footway,
            1,
            false,
            vec![(0.0, 100.0), (0.0, -600.0)],
        )];
        assert!(match_roads(&line, &roads).iter().all(Option::is_none));
    }

    #[test]
    fn should_wall_and_roof_the_route_through_a_tunnel_only() {
        let line = north_line(40);
        let mut ridden = Ridden::new(&line, &[]);
        let tunnel = Level {
            layer: -1,
            bridge: false,
            tunnel: true,
        };
        for l in &mut ridden.levels[10..20] {
            *l = tunnel;
        }
        let mesh = ridden_structures(&line, &ridden, |_, _| Some(0.0));
        assert!(!mesh.indices.is_empty());
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
        // Every drawn triangle is within the tunnel's stretch.
        for &i in &mesh.indices {
            let z = mesh.vertices[i as usize].pos[2];
            assert!(
                (-19.0 * 8.0 - 1e-3..=-10.0 * 8.0 + 1e-3).contains(&z),
                "{z}"
            );
        }
        let ceiling = mesh
            .vertices
            .iter()
            .map(|v| v.pos[1])
            .fold(f32::MIN, f32::max);
        assert!((ceiling - (10.0 + TUNNEL_HEIGHT_M + TUNNEL_SLAB_M)).abs() < 1e-3);
    }

    // ── The route over itself ─────────────────────────────────────────────────

    /// North for 400 m at 0 m elevation, round a loop, then back south over
    /// the same line at 8 m — an interchange in miniature.
    fn loop_line() -> Vec<RoadPoint> {
        let mut pts = Vec::new();
        let mut d = 0.0;
        for i in 0..50 {
            pts.push(RoadPoint {
                pos: [0.0, 0.0, -(i as f32) * 8.0],
                distance_m: d,
            });
            d += 8.0;
        }
        for i in 0..50 {
            // The loop: out east and back, climbing to 8 m.
            let t = i as f32 / 49.0;
            let a = std::f32::consts::PI * t;
            pts.push(RoadPoint {
                pos: [60.0 * a.sin(), 8.0 * t, -400.0 - 60.0 * a.sin()],
                distance_m: d,
            });
            d += 8.0;
        }
        for i in 0..50 {
            pts.push(RoadPoint {
                pos: [0.0, 8.0, -400.0 + (i as f32) * 8.0],
                distance_m: d,
            });
            d += 8.0;
        }
        pts
    }

    #[test]
    fn should_make_the_upper_pass_of_a_loop_a_bridge_and_leave_the_lower_alone() {
        let line = loop_line();
        let r = Ridden::new(&line, &[]);
        // The first pass, at ground level, is not a bridge anywhere.
        assert!(r.levels[..50].iter().all(|l| !l.bridge));
        // The return pass, 8 m up over it, is one along most of its length.
        let over = r.levels[100..150].iter().filter(|l| l.bridge).count();
        assert!(over >= 40, "only {over} of 50 upper points are a bridge");
        assert!(r.levels[100..150]
            .iter()
            .filter(|l| l.bridge)
            .all(|l| l.layer >= 1));
    }

    #[test]
    fn should_not_call_a_steep_climb_a_bridge_over_itself() {
        // A 15 % climb: every point is above the ones just behind it, but those
        // are the same pass, not another one underneath.
        let line: Vec<RoadPoint> = (0..100)
            .map(|i| RoadPoint {
                pos: [0.0, i as f32 * 1.2, -(i as f32) * 8.0],
                distance_m: i as f32 * 8.0,
            })
            .collect();
        assert!(Ridden::new(&line, &[]).levels.iter().all(|l| !l.bridge));
    }

    // ── A boulevard in many pieces ────────────────────────────────────────────

    #[test]
    fn should_stay_on_a_boulevard_mapped_in_pieces_past_a_slip_road() {
        // The boulevard in four ways, a slip road alongside it 1 m nearer the
        // GPS line than the boulevard is. The line stays on the boulevard,
        // which runs from 50 m south of the start to 750 m north of it.
        let line = north_line(90);
        let mut roads: Vec<LocalRoad> = (0..4)
            .map(|k| {
                let z0 = 50.0 - k as f32 * 200.0;
                local(
                    k,
                    RoadKind::Trunk,
                    3,
                    true,
                    vec![(-3.0, z0), (-3.0, z0 - 200.0)],
                )
            })
            .collect();
        let slip = LocalRoad {
            link: true,
            ..local(
                9,
                RoadKind::Trunk,
                1,
                true,
                vec![(2.0, -300.0), (2.0, -380.0)],
            )
        };
        roads.push(slip);
        let r = Ridden::new(&line, &roads);
        assert!(
            r.profiles.iter().all(|p| p.total == 3),
            "the road narrowed for the slip road"
        );
        assert!(!r.matched.contains(&9));
    }

    #[test]
    fn should_keep_lane_lines_on_the_tarmac_while_the_road_widens() {
        // Default road, then a three-lane carriageway: through the widening,
        // no paint may lie left of the asphalt's edge.
        let line = north_line(120);
        let roads = vec![local(
            1,
            RoadKind::Trunk,
            3,
            true,
            vec![(0.0, -480.0), (0.0, -1_200.0)],
        )];
        let r = Ridden::new(&line, &roads);
        let mesh = ridden_mesh(&line, &r);
        for v in mesh
            .vertices
            .iter()
            .filter(|v| v.colour[3] == MATERIAL_PLAIN)
        {
            let i = ((-v.pos[2]) / 8.0).round().clamp(0.0, 119.0) as usize;
            // The line runs north along x = 0, so x is the offset.
            assert!(
                v.pos[0] >= r.left[i.saturating_sub(1)].min(r.left[(i + 1).min(119)]) - 0.2,
                "paint at {:?} is off the tarmac (left edge {})",
                v.pos,
                r.left[i]
            );
        }
    }
}
