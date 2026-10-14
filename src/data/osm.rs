//! The world along a route, from OpenStreetMap: buildings, land use, water and
//! coastline for the 3D course.
//!
//! One Overpass API query per route fetches everything near the road, and the
//! answer is kept under `<data>/osm/` so a route pays the (slow — 45 s for 40
//! km of Istanbul) download once. Everything that arrives is untrusted: sizes
//! are capped, coordinates checked, rings validated, and anything malformed is
//! dropped rather than drawn.
//!
//! Map data © OpenStreetMap contributors, under the Open Database License; the
//! view credits it while it is on screen.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::data::course3d::LocalFrame;
use crate::data::route::Route;

/// Buildings are fetched this close to the road, in metres. Past this the
/// haze has them, and in a city every extra metre is hundreds more.
pub const BUILDING_REACH_M: u32 = 250;

/// Land use and water are fetched this close — the whole ground the view draws.
pub const AREA_REACH_M: u32 = 1_200;

/// Coastline reaches a little further, so ground at the edge of the view can
/// still tell which side of the shore it is on.
pub const COAST_REACH_M: u32 = 1_500;

/// The route is simplified to within this many metres before it goes into the
/// query. Far below the reach of anything fetched, and it keeps a 100 km ride
/// to a few hundred points.
const SIMPLIFY_M: f64 = 30.0;

/// Most points the query's line may carry. A route that will not simplify
/// below this is not a ride.
const MAX_QUERY_POINTS: usize = 1_500;

/// Largest answer accepted, from the network or the cache, in bytes. 40 km of
/// Istanbul is under 5 MB.
pub const MAX_RESPONSE_BYTES: u64 = 64 * 1_048_576;

/// Most elements, and most points in one element, accepted from an answer.
const MAX_ELEMENTS: usize = 200_000;
const MAX_POINTS_PER_WAY: usize = 20_000;

/// Public Overpass servers, tried in order. The main one is often overloaded
/// (it answered 504 three times running while this was written); the second
/// answered the same query in two minutes.
const OVERPASS_URLS: [&str; 2] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
];

/// The whole download. Overpass runs a large query for tens of seconds.
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);

/// A point as OpenStreetMap gives it.
pub type LatLng = (f64, f64);

/// What a stretch of ground is, for colouring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AreaKind {
    Water,
    Wood,
    Farmland,
    Grass,
    Sand,
    Urban,
    Residential,
}

impl AreaKind {
    /// Which kind wins where two overlap: lower first. Water over everything,
    /// so a pond in a park is a pond; residential last, since it is drawn
    /// around whole towns and other areas sit inside it.
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// The kind an element's tags describe, if it is one the view draws.
    fn from_tags(tags: &Tags) -> Option<Self> {
        let get = |k: &str| tags.get(k).map(String::as_str);
        if get("natural") == Some("water")
            || matches!(get("landuse"), Some("reservoir" | "basin"))
            || get("waterway") == Some("riverbank")
        {
            return Some(Self::Water);
        }
        match (get("natural"), get("landuse"), get("leisure")) {
            (Some("wood"), _, _) | (_, Some("forest"), _) => Some(Self::Wood),
            (Some("beach" | "sand"), _, _) => Some(Self::Sand),
            (Some("grassland" | "heath" | "scrub" | "wetland"), _, _) => Some(Self::Grass),
            (_, Some("farmland" | "orchard" | "vineyard" | "greenhouse_horticulture"), _) => {
                Some(Self::Farmland)
            }
            (
                _,
                Some("grass" | "meadow" | "recreation_ground" | "village_green" | "cemetery"),
                _,
            )
            | (_, _, Some("park" | "pitch" | "golf_course")) => Some(Self::Grass),
            (
                _,
                Some(
                    "commercial" | "industrial" | "retail" | "construction" | "brownfield"
                    | "railway" | "military" | "garages" | "religious",
                ),
                _,
            ) => Some(Self::Urban),
            (_, Some("residential"), _) => Some(Self::Residential),
            _ => None,
        }
    }
}

/// What kind of road a way is, by its `highway` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoadKind {
    Motorway,
    Trunk,
    Primary,
    Secondary,
    Tertiary,
    /// Unclassified, residential and living streets.
    Minor,
    Service,
    Cycleway,
    /// A footbridge or steps over a road: drawn, never ridden.
    Footway,
}

impl RoadKind {
    /// The kind a `highway` value names, and whether it is a link (slip) road.
    fn from_highway(value: &str) -> Option<(Self, bool)> {
        let (base, link) = match value.strip_suffix("_link") {
            Some(base) => (base, true),
            None => (value, false),
        };
        let kind = match base {
            "motorway" => Self::Motorway,
            "trunk" => Self::Trunk,
            "primary" => Self::Primary,
            "secondary" => Self::Secondary,
            "tertiary" => Self::Tertiary,
            "unclassified" | "residential" | "living_street" if !link => Self::Minor,
            "service" if !link => Self::Service,
            "cycleway" if !link => Self::Cycleway,
            "footway" | "path" | "pedestrian" | "steps" if !link => Self::Footway,
            _ => return None,
        };
        Some((kind, link))
    }
}

/// A road from the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Road {
    pub id: u64,
    pub kind: RoadKind,
    /// A slip road joining two others: one lane unless tagged otherwise.
    pub link: bool,
    /// Lanes in total, both directions, if the mapper said.
    pub lanes: Option<u8>,
    /// Traffic only runs in the order of `points`.
    pub oneway: bool,
    /// Which level it is on where roads cross: above zero over others,
    /// below in a tunnel or underpass.
    pub layer: i8,
    pub bridge: bool,
    pub tunnel: bool,
    pub points: Vec<LatLng>,
}

/// A building's footprint and how tall it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct Building {
    pub id: u64,
    /// Closed ring, last point equal to the first.
    pub outline: Vec<LatLng>,
    /// Tagged, or worked out from storeys or type — see [`building_height`].
    pub height_m: f32,
    /// The height was tagged, and so includes the roof.
    pub height_tagged: bool,
    /// The storeys were tagged. With neither this nor a height, `height_m`
    /// is a guess the scene may replace with a better one for its region.
    pub levels_tagged: bool,
    pub kind: crate::data::roofs::BuildingKind,
    pub roof: crate::data::roofs::RoofTags,
}

/// A stretch of land use or water: one or more rings, read even-odd, so an
/// inner ring (an island in a lake) is a hole.
#[derive(Debug, Clone, PartialEq)]
pub struct Area {
    pub kind: AreaKind,
    pub rings: Vec<Vec<LatLng>>,
}

/// Everything along a route the view draws from the map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    pub buildings: Vec<Building>,
    pub roads: Vec<Road>,
    pub areas: Vec<Area>,
    /// Coastline, each way in its own order: land on the left, water on the
    /// right, by OpenStreetMap's convention.
    pub coast: Vec<Vec<LatLng>>,
}

type Tags = std::collections::HashMap<String, String>;

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    elements: Vec<Element>,
    /// Overpass reports a query that ran out of time or memory here — with a
    /// 200 status and whatever it had found so far.
    #[serde(default)]
    remark: Option<String>,
}

#[derive(Deserialize)]
struct Element {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    tags: Tags,
    #[serde(default)]
    geometry: Vec<Option<Point>>,
    #[serde(default)]
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    #[serde(default)]
    role: String,
    #[serde(default)]
    geometry: Vec<Option<Point>>,
}

#[derive(Deserialize, Clone, Copy)]
struct Point {
    lat: f64,
    lon: f64,
}

/// A point that is somewhere on Earth, or `None`.
fn checked(p: &Option<Point>) -> Option<LatLng> {
    let p = (*p)?;
    (p.lat.is_finite()
        && p.lon.is_finite()
        && (-90.0..=90.0).contains(&p.lat)
        && (-180.0..=180.0).contains(&p.lon))
    .then_some((p.lat, p.lon))
}

/// A way's points, if every one is real and there are not absurdly many.
fn line(geometry: &[Option<Point>]) -> Option<Vec<LatLng>> {
    if geometry.len() > MAX_POINTS_PER_WAY {
        return None;
    }
    geometry.iter().map(checked).collect()
}

/// A closed ring with an area to it: at least three distinct corners and the
/// last point back on the first.
fn is_ring(points: &[LatLng]) -> bool {
    points.len() >= 4 && points.first() == points.last()
}

/// Join a relation's member ways into closed rings, end to end, either way
/// round. Members that never close are dropped: an open ring has no inside.
fn assemble_rings(mut pieces: Vec<Vec<LatLng>>) -> Vec<Vec<LatLng>> {
    let mut rings = Vec::new();
    pieces.retain(|p| p.len() >= 2);
    while let Some(mut ring) = pieces.pop() {
        loop {
            if is_ring(&ring) {
                rings.push(ring);
                break;
            }
            let end = *ring.last().expect("at least two points");
            let Some(i) = pieces
                .iter()
                .position(|p| p.first() == Some(&end) || p.last() == Some(&end))
            else {
                break; // never closes
            };
            let mut next = pieces.swap_remove(i);
            if next.first() != Some(&end) {
                next.reverse();
            }
            ring.extend_from_slice(&next[1..]);
        }
    }
    rings
}

/// Metres in an OpenStreetMap `height` value: "12", "12 m", "12.5m", "12,5".
/// Anything else — feet, ranges, words, nonsense — is `None`.
fn parse_height(raw: &str) -> Option<f32> {
    let s = raw.trim().trim_end_matches('m').trim().replace(',', ".");
    let h: f32 = s.parse().ok()?;
    (h.is_finite() && h > 0.0 && h <= 600.0).then_some(h)
}

/// Height of a storey, in metres.
const STOREY_M: f32 = 3.0;

/// How tall to draw a building: its own height, else its storeys, else a
/// guess from what it is. Guesses vary by a storey either way, seeded by the
/// building's id, so a street is not one flat block — and is the same street
/// every ride.
pub fn building_height(id: u64, tags: &Tags) -> f32 {
    if let Some(h) = tags.get("height").and_then(|h| parse_height(h)) {
        return h;
    }
    if let Some(levels) = tags
        .get("building:levels")
        .and_then(|l| l.trim().parse::<f32>().ok())
        .filter(|l| l.is_finite() && *l >= 1.0 && *l <= 200.0)
    {
        return levels * STOREY_M;
    }
    let storeys: f32 = match tags.get("building").map(String::as_str) {
        Some("apartments") => 6.0,
        Some("house" | "detached" | "semidetached_house" | "terrace" | "bungalow") => 2.0,
        Some("garage" | "garages" | "shed" | "hut" | "roof" | "carport" | "greenhouse") => 1.0,
        Some("commercial" | "office" | "retail" | "hospital" | "hotel") => 4.0,
        Some("industrial" | "warehouse" | "school" | "train_station") => 2.0,
        _ => 3.0,
    };
    // −1, 0 or +1 storey, never below one.
    let nudge = (splitmix(id) % 3) as f32 - 1.0;
    (storeys + nudge).max(1.0) * STOREY_M
}

/// A well-mixed hash of a building id, for its variations.
pub fn splitmix(id: u64) -> u64 {
    let mut z = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Read a road from a way's tags and points, if it is one the view draws.
///
/// `oneway=-1` means traffic runs against the way's order, so the points are
/// turned round to match: afterwards `oneway` always means "in point order".
/// Motorways and roundabouts are one-way without saying so. A footway is only
/// wanted when it is a bridge — a footbridge over the road is part of the
/// view; the hundreds of pavements beside it are not.
fn road(id: u64, tags: &Tags, mut points: Vec<LatLng>) -> Option<Road> {
    let get = |k: &str| tags.get(k).map(String::as_str);
    let (kind, link) = RoadKind::from_highway(get("highway")?)?;
    let flag = |k: &str| matches!(get(k), Some(v) if v != "no");
    let (bridge, tunnel) = (flag("bridge"), flag("tunnel"));
    if points.len() < 2 || get("area") == Some("yes") || (kind == RoadKind::Footway && !bridge) {
        return None;
    }
    // A bridge is over what it crosses unless it says otherwise, a tunnel
    // under; out-of-range layers are clamped to what the view can tell apart.
    let layer = get("layer")
        .and_then(|l| l.trim().parse::<i8>().ok())
        .map(|l| l.clamp(-5, 5))
        .unwrap_or(match (bridge, tunnel) {
            (true, _) => 1,
            (_, true) => -1,
            _ => 0,
        });
    let oneway = match get("oneway") {
        Some("yes" | "1" | "true") => true,
        Some("-1" | "reverse") => {
            points.reverse();
            true
        }
        Some("no" | "false" | "0") => false,
        _ => kind == RoadKind::Motorway || get("junction") == Some("roundabout"),
    };
    let lanes = get("lanes")
        .and_then(|l| l.trim().parse::<u8>().ok())
        .filter(|l| (1..=16).contains(l));
    Some(Road {
        id,
        kind,
        link,
        lanes,
        oneway,
        layer,
        bridge,
        tunnel,
        points,
    })
}

/// Read an Overpass answer. Elements that are malformed, unknown or too large
/// are skipped; only an answer that is not JSON at all is an error.
pub fn parse(bytes: &[u8]) -> Result<Features> {
    let response: Response = serde_json::from_slice(bytes).context("map data is not JSON")?;
    // A partial answer is worse than none: kept, it would leave the route
    // half built for good.
    if let Some(remark) = response.remark.as_deref().filter(|r| r.contains("error")) {
        anyhow::bail!("map data incomplete: {remark}");
    }
    anyhow::ensure!(
        response.elements.len() <= MAX_ELEMENTS,
        "map data has {} elements",
        response.elements.len()
    );
    let mut features = Features::default();
    for e in &response.elements {
        // Every ring this element describes.
        let rings: Vec<Vec<LatLng>> = match e.kind.as_str() {
            "way" => match line(&e.geometry) {
                Some(l) if e.tags.get("natural").map(String::as_str) == Some("coastline") => {
                    if l.len() >= 2 {
                        features.coast.push(l);
                    }
                    continue;
                }
                Some(l) if e.tags.contains_key("highway") => {
                    if let Some(r) = road(e.id, &e.tags, l) {
                        features.roads.push(r);
                    }
                    continue;
                }
                Some(l) if is_ring(&l) => vec![l],
                _ => continue,
            },
            "relation" => {
                let pieces: Vec<Vec<LatLng>> = e
                    .members
                    .iter()
                    .filter(|m| matches!(m.role.as_str(), "outer" | "inner" | ""))
                    .filter_map(|m| line(&m.geometry))
                    .collect();
                assemble_rings(pieces)
            }
            _ => continue,
        };
        if rings.is_empty() {
            continue;
        }
        if e.tags.contains_key("building") {
            // A building relation draws its first ring; its holes are courtyards
            // too small to matter from the road.
            let outline = rings.into_iter().next().expect("not empty");
            let tag = |k: &str| e.tags.get(k).map(String::as_str);
            features.buildings.push(Building {
                id: e.id,
                outline,
                height_m: building_height(e.id, &e.tags),
                height_tagged: tag("height").and_then(parse_height).is_some(),
                levels_tagged: tag("building:levels")
                    .and_then(|l| l.trim().parse::<f32>().ok())
                    .is_some_and(|l| l.is_finite() && (1.0..=200.0).contains(&l)),
                kind: crate::data::roofs::BuildingKind::from_tag(tag("building").unwrap_or("")),
                roof: crate::data::roofs::RoofTags {
                    shape: tag("roof:shape").and_then(crate::data::roofs::RoofShape::from_tag),
                    height_m: tag("roof:height")
                        .and_then(parse_height)
                        .filter(|h| *h <= 30.0),
                    colour: tag("roof:colour").and_then(crate::data::roofs::parse_colour),
                },
            });
        } else if let Some(kind) = AreaKind::from_tags(&e.tags) {
            features.areas.push(Area { kind, rings });
        }
    }
    Ok(features)
}

/// The route as few points as keep it within [`SIMPLIFY_M`] (Douglas–Peucker).
pub fn simplify(route: &Route) -> Vec<LatLng> {
    let pts: Vec<LatLng> = route.points.iter().map(|p| (p.lat, p.lng)).collect();
    let Some(frame) = LocalFrame::around(route) else {
        return Vec::new();
    };
    let xy: Vec<(f64, f64)> = pts
        .iter()
        .map(|&(lat, lng)| frame.to_local(lat, lng))
        .collect();
    let mut keep = vec![false; pts.len()];
    if let Some(last) = keep.len().checked_sub(1) {
        keep[0] = true;
        keep[last] = true;
    }
    // An explicit stack: a 10 000-point GPX must not recurse 10 000 deep.
    let mut stack = vec![(0usize, pts.len().saturating_sub(1))];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (ax, ay) = xy[a];
        let (bx, by) = xy[b];
        let (dx, dy) = (bx - ax, by - ay);
        let len = (dx * dx + dy * dy).sqrt();
        let mut worst = (0.0f64, a);
        for (i, &(px, py)) in xy.iter().enumerate().take(b).skip(a + 1) {
            let d = if len < 1e-9 {
                ((px - ax).powi(2) + (py - ay).powi(2)).sqrt()
            } else {
                (dx * (ay - py) - (ax - px) * dy).abs() / len
            };
            if d > worst.0 {
                worst = (d, i);
            }
        }
        if worst.0 > SIMPLIFY_M {
            keep[worst.1] = true;
            stack.push((a, worst.1));
            stack.push((worst.1, b));
        }
    }
    pts.into_iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(p))
        .collect()
}

/// The route asked for more than any ride needs.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("route simplifies to {0} points, more than the {MAX_QUERY_POINTS} a query may carry")]
pub struct RouteTooComplex(pub usize);

/// The route as the query gives it: its simplified points, `lat,lng,…`.
fn query_line(route: &Route) -> Result<String, RouteTooComplex> {
    let pts = simplify(route);
    if pts.len() > MAX_QUERY_POINTS {
        return Err(RouteTooComplex(pts.len()));
    }
    Ok(pts
        .iter()
        .map(|(lat, lng)| format!("{lat:.5},{lng:.5}"))
        .collect::<Vec<_>>()
        .join(","))
}

/// What names a route's map data in the cache: its own simplified line, not
/// the query around it. A new version of the query — another kind of feature
/// fetched — then finds the route's old answer instead of orphaning it, and a
/// rider offline still has their route's world.
pub fn route_key(route: &Route) -> Result<u64, RouteTooComplex> {
    query_line(route).map(|line| stable_hash(&line))
}

/// The Overpass query for everything the view draws along `route`.
pub fn query(route: &Route) -> Result<String, RouteTooComplex> {
    let line = query_line(route)?;
    let b = format!("around:{BUILDING_REACH_M},{line}");
    let a = format!("around:{AREA_REACH_M},{line}");
    let c = format!("around:{COAST_REACH_M},{line}");
    Ok(format!(
        "[out:json][timeout:170];(\
         way[building]({b});relation[building]({b});\
         way[landuse]({a});relation[landuse]({a});\
         way[natural~\"^(wood|water|grassland|scrub|heath|wetland|beach|sand)$\"]({a});\
         relation[natural~\"^(wood|water|wetland)$\"]({a});\
         way[leisure~\"^(park|pitch|golf_course)$\"]({a});\
         way[highway~\"^(motorway|trunk|primary|secondary|tertiary)(_link)?$\"]({b});\
         way[highway~\"^(unclassified|residential|living_street|service|cycleway)$\"]({b});\
         way[highway~\"^(footway|path|pedestrian|steps)$\"][bridge]({b});\
         way[natural=coastline]({c}););out tags geom;"
    ))
}

/// The map-data cache directory.
pub fn cache_dir() -> PathBuf {
    crate::data::paths::data_dir().join("osm")
}

/// FNV-1a: a hash that, unlike std's, is the same in every build, so a cache
/// file written by one version is found by the next.
pub fn stable_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Where the answer to `query` is kept under `root`.
pub fn cache_path(root: &Path, query: &str) -> PathBuf {
    root.join(format!("{:016x}.json", stable_hash(query)))
}

/// A cached answer, if there is a usable one.
pub fn read_cached(root: &Path, query: &str) -> Option<Vec<u8>> {
    let path = cache_path(root, query);
    if std::fs::metadata(&path).ok()?.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Keep an answer the way caches were kept before answers were keyed by
/// route — what [`load`] adopts. Only tests write this form now.
#[cfg(test)]
pub fn store(root: &Path, query: &str, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_RESPONSE_BYTES,
        "map data too large"
    );
    std::fs::create_dir_all(root).context("creating the map-data cache")?;
    let path = cache_path(root, query);
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, bytes).context("writing map data")?;
    std::fs::rename(&tmp, &path).context("keeping map data")?;
    Ok(())
}

/// Where a route's map data is kept, and the version of the query it answers.
fn route_paths(root: &Path, key: u64) -> (PathBuf, PathBuf) {
    (
        root.join(format!("route-{key:016x}.json")),
        root.join(format!("route-{key:016x}.version")),
    )
}

/// How a route's cached map data stands against the current query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cached {
    /// Answers the current query.
    Current,
    /// Answers an older query: usable — offline it is all there is — but
    /// replaced when a download succeeds.
    Outdated,
    Missing,
}

/// The state of `route`'s map data in the cache under `root`.
pub fn cached_state(root: &Path, route: &Route) -> Cached {
    let (Ok(key), Ok(query)) = (route_key(route), query(route)) else {
        return Cached::Missing;
    };
    let (data, version) = route_paths(root, key);
    if !std::fs::metadata(&data).is_ok_and(|m| m.len() <= MAX_RESPONSE_BYTES) {
        return Cached::Missing;
    }
    let current = format!("{:016x}", stable_hash(&query));
    match std::fs::read_to_string(version) {
        Ok(v) if v.trim() == current => Cached::Current,
        _ => Cached::Outdated,
    }
}

/// A route's cached answer and the query version it answers, if there is one.
fn read_route(root: &Path, key: u64) -> Option<(Vec<u8>, String)> {
    let (data, version) = route_paths(root, key);
    if std::fs::metadata(&data).ok()?.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    let bytes = std::fs::read(data).ok()?;
    let version = std::fs::read_to_string(version).unwrap_or_default();
    Some((bytes, version.trim().to_string()))
}

/// Keep a route's answer and the query version it answers. Each file goes in
/// beside its name and is renamed into place; if the version cannot follow
/// the data, the data reads as outdated — still used, never lost.
fn store_route(root: &Path, key: u64, query: &str, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_RESPONSE_BYTES,
        "map data too large"
    );
    std::fs::create_dir_all(root).context("creating the map-data cache")?;
    let (data, version) = route_paths(root, key);
    for (path, contents) in [
        (&data, bytes.to_vec()),
        (
            &version,
            format!("{:016x}", stable_hash(query)).into_bytes(),
        ),
    ] {
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, contents).context("writing map data")?;
        std::fs::rename(&tmp, path).context("keeping map data")?;
    }
    Ok(())
}

/// Background downloads — routes being prepared for offline — go one at a
/// time, app-wide: the public Overpass servers ask for no parallel requests.
/// A route opened in 3D does not queue behind them.
static BACKGROUND_QUEUE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A route's cached answer, if it answers `current` and still parses.
fn current_answer(root: &Path, key: u64, current: &str) -> Option<Features> {
    let (bytes, version) = read_route(root, key)?;
    if version != current {
        return None;
    }
    match parse(&bytes) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("Cached map data unreadable, fetching again: {e}");
            None
        }
    }
}

/// The features along `route`.
///
/// From the cache when it answers the current query. Otherwise fetched —
/// and if that fails, an older cached answer is used rather than nothing: a
/// rider offline after an app update still has their route's world. An
/// answer is only replaced once a new one has fully arrived and parsed.
///
/// For a route the rider has opened: never waits behind background downloads.
pub async fn load(root: PathBuf, route: Route) -> Result<Features> {
    load_inner(root, route, false).await
}

/// [`load`] for a route being prepared in the background: downloads queue
/// one at a time, app-wide. A route already in the cache is answered at once,
/// without queueing.
pub async fn load_queued(root: PathBuf, route: Route) -> Result<Features> {
    load_inner(root, route, true).await
}

async fn load_inner(root: PathBuf, route: Route, queued: bool) -> Result<Features> {
    let query = query(&route)?;
    let key = route_key(&route)?;
    let current = format!("{:016x}", stable_hash(&query));
    // The cache first, before any queue: a route on disk opens at once.
    if let Some(f) = current_answer(&root, key, &current) {
        return Ok(f);
    }
    let _in_turn = if queued {
        let guard = BACKGROUND_QUEUE.lock().await;
        // Whoever went before may have fetched this very route.
        if let Some(f) = current_answer(&root, key, &current) {
            return Ok(f);
        }
        Some(guard)
    } else {
        None
    };
    let cached = read_route(&root, key);
    // A cache written before answers were kept by route.
    if let Some(bytes) = read_cached(&root, &query) {
        if let Ok(f) = parse(&bytes) {
            if let Err(e) = store_route(&root, key, &query, &bytes) {
                tracing::warn!("Could not move map data to its route: {e}");
            }
            return Ok(f);
        }
    }
    match download(&query).await {
        Ok((bytes, features)) => {
            if let Err(e) = store_route(&root, key, &query, &bytes) {
                tracing::warn!("Could not cache map data: {e}");
            }
            Ok(features)
        }
        Err(e) => match cached.and_then(|(bytes, _)| parse(&bytes).ok()) {
            Some(older) => {
                tracing::warn!("Map data not refreshed ({e:#}); using the route's older copy");
                Ok(older)
            }
            None => Err(e),
        },
    }
}

/// Ask each server in turn for `query`.
async fn download(query: &str) -> Result<(Vec<u8>, Features)> {
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "TourDuSalon/",
            env!("CARGO_PKG_VERSION"),
            " (io.github.rorynuijens.Cycle)"
        ))
        .build()
        .context("starting the map-data download")?;
    let mut last_error = anyhow::anyhow!("no map-data server tried");
    for url in OVERPASS_URLS {
        match fetch(&client, url, query).await {
            Ok((bytes, features)) => {
                tracing::info!(
                    "Map data ready ({} buildings, {} areas, {} roads, {} coastline ways)",
                    features.buildings.len(),
                    features.areas.len(),
                    features.roads.len(),
                    features.coast.len()
                );
                return Ok((bytes, features));
            }
            Err(e) => {
                tracing::warn!("Map data from {url} failed: {e:#}");
                last_error = e;
            }
        }
    }
    Err(last_error)
}

/// One server's answer to `query`, checked and read.
async fn fetch(client: &reqwest::Client, url: &str, query: &str) -> Result<(Vec<u8>, Features)> {
    let response = client
        .post(url)
        .form(&[("data", query)])
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .context("requesting map data")?
        .error_for_status()
        .context("map data refused")?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES)
    {
        anyhow::bail!("map data too large");
    }
    let bytes = response.bytes().await.context("reading map data")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_RESPONSE_BYTES,
        "map data too large"
    );
    let features = parse(&bytes)?;
    Ok((bytes.to_vec(), features))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    fn tags(pairs: &[(&str, &str)]) -> Tags {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── Heights ───────────────────────────────────────────────────────────────

    #[test]
    fn should_read_heights_as_mappers_write_them() {
        assert_eq!(parse_height("12"), Some(12.0));
        assert_eq!(parse_height("12 m"), Some(12.0));
        assert_eq!(parse_height("12.5m"), Some(12.5));
        assert_eq!(parse_height("12,5"), Some(12.5));
        assert_eq!(parse_height(" 7 "), Some(7.0));
    }

    #[test]
    fn should_refuse_heights_that_are_not_metres_on_earth() {
        for raw in [
            "", "tall", "40'", "10-12", "0", "-3", "1e9", "NaN", "inf", "601",
        ] {
            assert_eq!(parse_height(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn should_prefer_height_then_storeys_then_a_guess() {
        let t = tags(&[
            ("building", "yes"),
            ("height", "20"),
            ("building:levels", "2"),
        ]);
        assert_eq!(building_height(1, &t), 20.0);
        let t = tags(&[("building", "yes"), ("building:levels", "5")]);
        assert_eq!(building_height(1, &t), 15.0);
        let t = tags(&[("building", "yes"), ("building:levels", "lots")]);
        let guess = building_height(1, &t);
        assert!([6.0, 9.0, 12.0].contains(&guess), "{guess}");
    }

    #[test]
    fn should_guess_apartments_taller_than_houses_whatever_the_nudge() {
        for id in 0..200 {
            let flats = building_height(id, &tags(&[("building", "apartments")]));
            let house = building_height(id, &tags(&[("building", "house")]));
            assert!((15.0..=21.0).contains(&flats), "{flats}");
            assert!((3.0..=9.0).contains(&house), "{house}");
        }
    }

    #[test]
    fn should_never_guess_below_one_storey() {
        for id in 0..200 {
            assert!(building_height(id, &tags(&[("building", "shed")])) >= STOREY_M);
        }
    }

    #[test]
    fn should_guess_the_same_height_for_the_same_building_every_time() {
        let t = tags(&[("building", "yes")]);
        assert_eq!(
            building_height(4_500_993, &t),
            building_height(4_500_993, &t)
        );
        // And not all the same along a street.
        let heights: std::collections::BTreeSet<u32> =
            (0..50).map(|id| building_height(id, &t) as u32).collect();
        assert_eq!(heights.len(), 3);
    }

    // ── Kinds ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_sort_tags_into_the_kinds_the_view_draws() {
        let k = |pairs: &[(&str, &str)]| AreaKind::from_tags(&tags(pairs));
        assert_eq!(k(&[("natural", "water")]), Some(AreaKind::Water));
        assert_eq!(k(&[("landuse", "reservoir")]), Some(AreaKind::Water));
        assert_eq!(k(&[("landuse", "forest")]), Some(AreaKind::Wood));
        assert_eq!(k(&[("natural", "wood")]), Some(AreaKind::Wood));
        assert_eq!(k(&[("landuse", "farmland")]), Some(AreaKind::Farmland));
        assert_eq!(k(&[("leisure", "park")]), Some(AreaKind::Grass));
        assert_eq!(k(&[("landuse", "construction")]), Some(AreaKind::Urban));
        assert_eq!(
            k(&[("landuse", "residential")]),
            Some(AreaKind::Residential)
        );
        assert_eq!(k(&[("natural", "beach")]), Some(AreaKind::Sand));
        assert_eq!(k(&[("amenity", "parking")]), None);
        assert_eq!(k(&[]), None);
    }

    #[test]
    fn should_let_water_win_over_a_park_it_sits_in() {
        assert!(AreaKind::Water.priority() < AreaKind::Grass.priority());
        assert!(AreaKind::Grass.priority() < AreaKind::Residential.priority());
    }

    // ── Parsing ───────────────────────────────────────────────────────────────

    fn square(lat: f64, lng: f64, d: f64) -> String {
        format!(
            r#"[{{"lat":{lat},"lon":{lng}}},{{"lat":{lat},"lon":{lng2}}},{{"lat":{lat2},"lon":{lng2}}},{{"lat":{lat2},"lon":{lng}}},{{"lat":{lat},"lon":{lng}}}]"#,
            lng2 = lng + d,
            lat2 = lat + d,
        )
    }

    #[test]
    fn should_read_buildings_areas_and_coast() {
        let json = format!(
            r#"{{"elements":[
              {{"type":"way","id":7,"tags":{{"building":"yes","building:levels":"4"}},"geometry":{b}}},
              {{"type":"way","id":8,"tags":{{"landuse":"forest"}},"geometry":{w}}},
              {{"type":"way","id":9,"tags":{{"natural":"coastline"}},"geometry":[{{"lat":41.0,"lon":29.0}},{{"lat":41.0,"lon":29.1}}]}},
              {{"type":"way","id":10,"tags":{{"amenity":"parking"}},"geometry":{w}}},
              {{"type":"node","id":11,"lat":41.0,"lon":29.0}}
            ]}}"#,
            b = square(41.0, 29.0, 0.0001),
            w = square(41.0, 29.0, 0.01),
        );
        let f = parse(json.as_bytes()).expect("parses");
        assert_eq!(f.buildings.len(), 1);
        assert_eq!(f.buildings[0].height_m, 12.0);
        assert_eq!(f.buildings[0].id, 7);
        assert_eq!(f.areas.len(), 1);
        assert_eq!(f.areas[0].kind, AreaKind::Wood);
        assert_eq!(f.coast.len(), 1);
    }

    #[test]
    fn should_assemble_a_relation_from_ways_in_any_direction() {
        // A lake in two halves, the second drawn backwards, with an island.
        let json = r#"{"elements":[{"type":"relation","id":1,"tags":{"natural":"water"},"members":[
            {"role":"outer","geometry":[{"lat":0,"lon":0},{"lat":0,"lon":1},{"lat":1,"lon":1}]},
            {"role":"outer","geometry":[{"lat":0,"lon":0},{"lat":1,"lon":0},{"lat":1,"lon":1}]},
            {"role":"inner","geometry":[{"lat":0.4,"lon":0.4},{"lat":0.4,"lon":0.6},{"lat":0.6,"lon":0.6},{"lat":0.4,"lon":0.4}]}
        ]}]}"#;
        let f = parse(json.as_bytes()).expect("parses");
        assert_eq!(f.areas.len(), 1);
        let rings = &f.areas[0].rings;
        assert_eq!(
            rings.len(),
            2,
            "outer joined into one ring, plus the island"
        );
        assert!(rings.iter().all(|r| is_ring(r)));
        assert!(rings.iter().any(|r| r.len() == 5), "{rings:?}");
    }

    #[test]
    fn should_drop_a_relation_whose_ways_never_close() {
        let json = r#"{"elements":[{"type":"relation","id":1,"tags":{"landuse":"forest"},"members":[
            {"role":"outer","geometry":[{"lat":0,"lon":0},{"lat":0,"lon":1}]},
            {"role":"outer","geometry":[{"lat":5,"lon":5},{"lat":6,"lon":6}]}
        ]}]}"#;
        assert!(parse(json.as_bytes()).expect("parses").areas.is_empty());
    }

    #[test]
    fn should_drop_elements_with_impossible_or_missing_points() {
        let json = r#"{"elements":[
            {"type":"way","id":1,"tags":{"building":"yes"},"geometry":[{"lat":0,"lon":0},{"lat":95,"lon":0},{"lat":1,"lon":1},{"lat":0,"lon":0}]},
            {"type":"way","id":2,"tags":{"building":"yes"},"geometry":[{"lat":0,"lon":0},null,{"lat":1,"lon":1},{"lat":0,"lon":0}]},
            {"type":"way","id":3,"tags":{"building":"yes"},"geometry":[{"lat":0,"lon":0},{"lat":1,"lon":1}]},
            {"type":"way","id":4,"tags":{"building":"yes"}}
        ]}"#;
        assert!(parse(json.as_bytes()).expect("parses").buildings.is_empty());
    }

    #[test]
    fn should_refuse_an_answer_cut_short_by_the_server() {
        // Overpass answers 200 with what it had when it ran out of time.
        let json = r#"{"elements":[],"remark":"runtime error: Query timed out in \"query\" at line 1 after 171 seconds."}"#;
        let e = parse(json.as_bytes()).expect_err("a partial answer");
        assert!(e.to_string().contains("incomplete"), "{e}");
        // A remark that is not an error is fine.
        assert!(parse(br#"{"elements":[],"remark":"note"}"#).is_ok());
    }

    #[test]
    fn should_refuse_an_answer_that_is_not_json() {
        assert!(parse(b"<html>rate limited</html>").is_err());
        assert_eq!(parse(b"{}").expect("an empty answer"), Features::default());
    }

    proptest::proptest! {
        #[test]
        fn should_never_panic_on_any_bytes(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..4000)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn should_never_panic_on_any_relation(
            ways in proptest::collection::vec(
                proptest::collection::vec((-1i8..3, -1i8..3), 0..6), 0..6)
        ) {
            let members: Vec<String> = ways.iter().map(|w| {
                let pts: Vec<String> = w.iter()
                    .map(|(a, b)| format!(r#"{{"lat":{a},"lon":{b}}}"#)).collect();
                format!(r#"{{"role":"outer","geometry":[{}]}}"#, pts.join(","))
            }).collect();
            let json = format!(
                r#"{{"elements":[{{"type":"relation","id":1,"tags":{{"natural":"water"}},"members":[{}]}}]}}"#,
                members.join(",")
            );
            let f = parse(json.as_bytes()).expect("valid JSON");
            for a in f.areas { for r in a.rings { proptest::prop_assert!(is_ring(&r)); } }
        }
    }

    // ── Query ─────────────────────────────────────────────────────────────────

    fn wiggly_route(points: usize) -> Route {
        let frame = LocalFrame::at(41.0, 29.0);
        let pts: Vec<RoutePoint> = (0..points)
            .map(|i| {
                let n = i as f64 * 8.0;
                let e = (n / 200.0).sin() * 150.0;
                let (lat, lng) = frame.to_geo(e, n);
                RoutePoint {
                    lat,
                    lng,
                    elevation_m: 0.0,
                    distance_m: n as f32,
                    gradient: 0.0,
                }
            })
            .collect();
        Route {
            name: "Wiggle".into(),
            total_distance_m: pts.last().map(|p| p.distance_m).unwrap_or(0.0),
            total_gain_m: 0.0,
            points: pts,
        }
    }

    #[test]
    fn should_simplify_without_straying_from_the_route() {
        let route = wiggly_route(2_000);
        let kept = simplify(&route);
        assert!(kept.len() < 400, "{}", kept.len());
        assert_eq!(
            kept.first(),
            Some(&(route.points[0].lat, route.points[0].lng))
        );
        let last = route.points.last().expect("points");
        assert_eq!(kept.last(), Some(&(last.lat, last.lng)));
        // Every original point within the tolerance of the simplified line.
        let frame = LocalFrame::around(&route).expect("points");
        let xy: Vec<(f64, f64)> = kept.iter().map(|&(a, b)| frame.to_local(a, b)).collect();
        for p in &route.points {
            let (px, py) = frame.to_local(p.lat, p.lng);
            let nearest = xy
                .windows(2)
                .map(|w| {
                    let ((ax, ay), (bx, by)) = (w[0], w[1]);
                    let (dx, dy) = (bx - ax, by - ay);
                    let t =
                        (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                    ((ax + t * dx - px).powi(2) + (ay + t * dy - py).powi(2)).sqrt()
                })
                .fold(f64::MAX, f64::min);
            assert!(nearest <= SIMPLIFY_M + 1e-6, "{nearest}");
        }
    }

    #[test]
    fn should_simplify_tiny_routes_to_themselves() {
        assert!(simplify(&wiggly_route(0)).is_empty());
        assert_eq!(simplify(&wiggly_route(1)).len(), 1);
        assert_eq!(simplify(&wiggly_route(2)).len(), 2);
    }

    #[test]
    fn should_ask_for_buildings_near_and_land_far() {
        let q = query(&wiggly_route(200)).expect("a query");
        assert!(q.starts_with("[out:json]"));
        assert!(q.contains(&format!("way[building](around:{BUILDING_REACH_M},")));
        assert!(q.contains(&format!("way[landuse](around:{AREA_REACH_M},")));
        assert!(q.contains(&format!("way[natural=coastline](around:{COAST_REACH_M},")));
        assert!(q.ends_with("out tags geom;"));
    }

    // ── Cache ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_hash_the_same_in_every_build() {
        // FNV-1a's published test value for "a".
        assert_eq!(stable_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(stable_hash(""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn should_keep_an_answer_and_find_it_again_by_its_query() {
        let root = std::env::temp_dir().join(format!("cycle-osm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        store(&root, "q1", b"{\"elements\":[]}").expect("stored");
        assert_eq!(
            read_cached(&root, "q1").as_deref(),
            Some(&b"{\"elements\":[]}"[..])
        );
        assert_eq!(read_cached(&root, "q2"), None);
        assert!(cache_path(&root, "q1").starts_with(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Roads ─────────────────────────────────────────────────────────────────

    fn two_points() -> Vec<LatLng> {
        vec![(41.0, 29.0), (41.001, 29.0)]
    }

    #[test]
    fn should_read_a_three_lane_one_way_boulevard() {
        let t = tags(&[("highway", "trunk"), ("lanes", "3"), ("oneway", "yes")]);
        let r = road(1, &t, two_points()).expect("a road");
        assert_eq!(
            (r.kind, r.link, r.lanes, r.oneway),
            (RoadKind::Trunk, false, Some(3), true)
        );
    }

    #[test]
    fn should_turn_a_reversed_one_way_round() {
        let t = tags(&[("highway", "primary"), ("oneway", "-1")]);
        let r = road(1, &t, two_points()).expect("a road");
        assert!(r.oneway);
        assert_eq!(r.points, vec![(41.001, 29.0), (41.0, 29.0)]);
    }

    #[test]
    fn should_know_motorways_and_roundabouts_are_one_way_unsaid() {
        let m = road(1, &tags(&[("highway", "motorway")]), two_points()).expect("a road");
        assert!(m.oneway);
        let r = road(
            1,
            &tags(&[("highway", "residential"), ("junction", "roundabout")]),
            two_points(),
        )
        .expect("a road");
        assert!(r.oneway);
        let s = road(1, &tags(&[("highway", "residential")]), two_points()).expect("a road");
        assert!(!s.oneway);
    }

    #[test]
    fn should_read_slip_roads_as_links_of_their_class() {
        let r = road(1, &tags(&[("highway", "trunk_link")]), two_points()).expect("a road");
        assert_eq!((r.kind, r.link), (RoadKind::Trunk, true));
        assert!(road(1, &tags(&[("highway", "residential_link")]), two_points()).is_none());
    }

    #[test]
    fn should_leave_out_squares_pavements_and_nonsense() {
        for t in [
            tags(&[("highway", "pedestrian"), ("area", "yes")]),
            tags(&[("highway", "footway")]),
            tags(&[("highway", "steps")]),
            tags(&[("highway", "footway"), ("bridge", "no")]),
            tags(&[("highway", "")]),
        ] {
            assert!(road(1, &t, two_points()).is_none(), "{t:?}");
        }
        assert!(road(1, &tags(&[("highway", "trunk")]), vec![(41.0, 29.0)]).is_none());
    }

    #[test]
    fn should_keep_a_footbridge() {
        let r = road(
            1,
            &tags(&[("highway", "footway"), ("bridge", "yes")]),
            two_points(),
        )
        .expect("a footbridge");
        assert_eq!((r.kind, r.bridge, r.layer), (RoadKind::Footway, true, 1));
    }

    #[test]
    fn should_put_bridges_over_and_tunnels_under_unless_told() {
        let level = |pairs: &[(&str, &str)]| {
            let r = road(1, &tags(pairs), two_points()).expect("a road");
            (r.layer, r.bridge, r.tunnel)
        };
        assert_eq!(level(&[("highway", "trunk")]), (0, false, false));
        assert_eq!(
            level(&[("highway", "trunk"), ("bridge", "yes")]),
            (1, true, false)
        );
        assert_eq!(
            level(&[("highway", "trunk"), ("bridge", "viaduct")]),
            (1, true, false)
        );
        assert_eq!(
            level(&[("highway", "trunk"), ("tunnel", "yes")]),
            (-1, false, true)
        );
        assert_eq!(
            level(&[("highway", "trunk"), ("tunnel", "no")]),
            (0, false, false)
        );
        assert_eq!(
            level(&[("highway", "trunk"), ("bridge", "yes"), ("layer", "2")]),
            (2, true, false)
        );
        // A layer no map uses is held to what can be told apart.
        assert_eq!(level(&[("highway", "trunk"), ("layer", "120")]).0, 5);
        assert_eq!(level(&[("highway", "trunk"), ("layer", "up")]).0, 0);
    }

    #[test]
    fn should_ignore_a_lane_count_no_road_has() {
        for lanes in ["0", "99", "three", "-2", ""] {
            let t = tags(&[("highway", "primary"), ("lanes", lanes)]);
            assert_eq!(
                road(1, &t, two_points()).expect("a road").lanes,
                None,
                "{lanes}"
            );
        }
    }

    #[test]
    fn should_read_roads_out_of_an_answer_but_not_coastline_or_buildings() {
        let json = r#"{"elements":[
            {"type":"way","id":5,"tags":{"highway":"trunk","lanes":"3","oneway":"yes"},"geometry":[{"lat":41.0,"lon":29.0},{"lat":41.001,"lon":29.0}]},
            {"type":"way","id":6,"tags":{"highway":"footway"},"geometry":[{"lat":41.0,"lon":29.0},{"lat":41.001,"lon":29.0}]}
        ]}"#;
        let f = parse(json.as_bytes()).expect("parses");
        assert_eq!(f.roads.len(), 1);
        assert_eq!(f.roads[0].id, 5);
        assert!(f.buildings.is_empty() && f.areas.is_empty());
    }

    #[test]
    fn should_ask_for_roads_near_the_route() {
        let q = query(&wiggly_route(200)).expect("a query");
        assert!(q.contains("(_link)?$"), "{q}");
        assert!(q.contains("cycleway"));
        // Footways only as bridges.
        assert!(q.contains("steps)$\"][bridge]"), "{q}");
    }

    // ── The route's own cache ─────────────────────────────────────────────────

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cycle-osm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn should_name_a_route_by_its_line_not_its_query() {
        let r = wiggly_route(300);
        let key = route_key(&r).expect("a key");
        assert_eq!(route_key(&r), Ok(key), "the same route, the same key");
        assert_ne!(route_key(&wiggly_route(200)), Ok(key));
        // The key does not move when the query around the line changes.
        assert_eq!(key, stable_hash(&query_line(&r).expect("a line")));
    }

    #[test]
    fn should_know_a_current_an_outdated_and_a_missing_answer() {
        let root = temp_root("state");
        let r = wiggly_route(300);
        assert_eq!(cached_state(&root, &r), Cached::Missing);
        let key = route_key(&r).expect("a key");
        store_route(&root, key, &query(&r).expect("a query"), b"{}").expect("stored");
        assert_eq!(cached_state(&root, &r), Cached::Current);
        // An app update changes the query: the answer is now outdated, not gone.
        store_route(&root, key, "an older query", b"{}").expect("stored");
        assert_eq!(cached_state(&root, &r), Cached::Outdated);
        // A lost version file reads as outdated, never as missing.
        std::fs::remove_file(route_paths(&root, key).1).expect("removed");
        assert_eq!(cached_state(&root, &r), Cached::Outdated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn should_ride_on_a_current_answer_without_the_network() {
        let root = temp_root("current");
        let r = wiggly_route(300);
        let json = format!(
            r#"{{"elements":[{{"type":"way","id":7,"tags":{{"building":"yes"}},"geometry":{}}}]}}"#,
            square(41.0, 29.0, 0.0001)
        );
        store_route(
            &root,
            route_key(&r).expect("a key"),
            &query(&r).expect("q"),
            json.as_bytes(),
        )
        .expect("stored");
        // No server is reachable in a test: this only passes from the cache.
        let f = load(root.clone(), r).await.expect("from the cache");
        assert_eq!(f.buildings.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn should_adopt_a_cache_written_before_answers_were_kept_by_route() {
        let root = temp_root("legacy");
        let r = wiggly_route(300);
        let q = query(&r).expect("q");
        store(&root, &q, br#"{"elements":[]}"#).expect("stored the old way");
        load(root.clone(), r.clone())
            .await
            .expect("from the old cache");
        assert_eq!(cached_state(&root, &r), Cached::Current);
        let _ = std::fs::remove_dir_all(&root);
    }
}
