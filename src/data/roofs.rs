//! Roofs: what shape, how tall and what colour each building's roof is, and
//! the triangles for it.
//!
//! About 1 % of buildings carry a `roof:shape` tag; on the rider's own routes
//! it is 13 of Maltepe's 26 000 and 0.6 % of Oude IJsselstreek's. So the roof
//! is decided by rules — what the building is, how tall, and which country it
//! stands in — and a tag, where there is one, wins. See
//! `docs/3d-course-blueprint.md`, section 4.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::course3d::{Mesh, Vertex, MATERIAL_PLAIN, NO_UV};
use crate::data::osm::splitmix;

/// Where the route is, which decides what an untagged building's roof is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Pitched tile roofs on houses, farms and sheds.
    Netherlands,
    /// Flat-roofed blocks; hipped terracotta on small houses.
    Turkey,
    Elsewhere,
}

impl Region {
    /// The region a route centred on `lat`, `lng` rides in. A box per country
    /// is enough: the question is which rules suit the place, not borders.
    pub fn at(lat: f64, lng: f64) -> Self {
        if (50.7..=53.6).contains(&lat) && (3.3..=7.3).contains(&lng) {
            Self::Netherlands
        } else if (35.8..=42.2).contains(&lat) && (25.6..=44.9).contains(&lng) {
            Self::Turkey
        } else {
            Self::Elsewhere
        }
    }
}

/// What a building is, as far as its roof cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildingKind {
    House,
    Farm,
    Shed,
    Apartments,
    Commercial,
    Industrial,
    Religious,
    Other,
}

impl BuildingKind {
    /// The kind a `building` value names.
    pub fn from_tag(value: &str) -> Self {
        match value {
            "house" | "detached" | "semidetached_house" | "terrace" | "bungalow"
            | "residential" | "cabin" | "farmhouse" => Self::House,
            "farm" | "barn" | "farm_auxiliary" | "stable" | "cowshed" | "sty" => Self::Farm,
            "shed" | "garage" | "garages" | "hut" | "carport" => Self::Shed,
            "apartments" | "dormitory" => Self::Apartments,
            "commercial" | "office" | "retail" | "supermarket" | "hotel" | "hospital"
            | "school" | "university" | "public" | "civic" | "government" => Self::Commercial,
            "industrial" | "warehouse" | "factory" | "manufacture" | "hangar" => Self::Industrial,
            "church" | "chapel" | "mosque" | "cathedral" | "temple" | "synagogue" => {
                Self::Religious
            }
            _ => Self::Other,
        }
    }
}

/// The roof shapes the view draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoofShape {
    Flat,
    /// A ridge along the building's long side, a gable wall at each end.
    Gabled,
    /// A shorter ridge, sloping on all four sides.
    Hipped,
    /// Four slopes meeting at a point.
    Pyramidal,
}

impl RoofShape {
    /// The shape a `roof:shape` value names, read to what can be drawn: a
    /// gambrel or saltbox is near enough a gable, a mansard or half-hip a hip.
    /// Domes and onions wait for the landmarks; they read as flat until then.
    pub fn from_tag(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "flat" | "skillion" | "dome" | "onion" | "round" => Self::Flat,
            "gabled" | "gambrel" | "saltbox" | "double_saltbox" => Self::Gabled,
            "hipped" | "half-hipped" | "side_hipped" | "mansard" => Self::Hipped,
            "pyramidal" => Self::Pyramidal,
            _ => return None,
        })
    }
}

/// What the map says about a building's roof, where it says anything.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RoofTags {
    pub shape: Option<RoofShape>,
    pub height_m: Option<f32>,
    pub colour: Option<[f32; 3]>,
}

/// A colour as mappers write it: `#rrggbb`, `#rgb`, or a common name.
pub fn parse_colour(raw: &str) -> Option<[f32; 3]> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let digits: Vec<u32> = hex.chars().map(|c| c.to_digit(16)).collect::<Option<_>>()?;
        let channel = |v: u32| v as f32 / 255.0;
        return match digits.as_slice() {
            [r1, r2, g1, g2, b1, b2] => Some([
                channel(r1 * 16 + r2),
                channel(g1 * 16 + g2),
                channel(b1 * 16 + b2),
            ]),
            [r, g, b] => Some([channel(r * 17), channel(g * 17), channel(b * 17)]),
            _ => None,
        };
    }
    Some(match s.as_str() {
        "red" => [0.62, 0.22, 0.17],
        "darkred" | "maroon" => [0.45, 0.17, 0.14],
        "brown" => [0.45, 0.30, 0.20],
        "orange" => [0.75, 0.45, 0.25],
        "black" => [0.14, 0.14, 0.15],
        "grey" | "gray" => [0.50, 0.50, 0.50],
        "darkgrey" | "darkgray" | "anthracite" => [0.26, 0.26, 0.28],
        "lightgrey" | "lightgray" | "silver" => [0.72, 0.72, 0.72],
        "white" => [0.90, 0.90, 0.88],
        "green" => [0.30, 0.42, 0.30],
        "blue" => [0.25, 0.32, 0.45],
        _ => return None,
    })
}

/// Height of a storey, in metres.
const STOREY_M: f32 = 3.0;
/// A Dutch barn or farm shed: one tall storey under a big roof.
const BARN_WALL_M: f32 = 4.5;
/// An untagged Dutch building with a footprint this large is a barn or a
/// shed, not a house.
const BARN_FROM_M2: f32 = 250.0;

/// The wall height of a building with neither height nor storeys tagged,
/// where the region knows better than a world-wide guess. `None` keeps the
/// generic guess.
///
/// Almost half of Montferland's 10 907 buildings carry no type at all, and a
/// world-wide three storeys made the Achterhoek a town of flat-roofed blocks.
/// There, an untagged building is a house or a farm building.
pub fn regional_walls(
    id: u64,
    kind: BuildingKind,
    region: Region,
    footprint_m2: f32,
) -> Option<f32> {
    if region != Region::Netherlands {
        return None;
    }
    // 1, 2, 2 or 3 storeys: mostly two, as Dutch houses are.
    let storeys = match splitmix(id ^ 0x57_0E_E5) % 4 {
        0 => 1.0,
        3 => 3.0,
        _ => 2.0,
    };
    Some(match kind {
        BuildingKind::Farm => BARN_WALL_M,
        BuildingKind::Shed => STOREY_M,
        BuildingKind::House => storeys * STOREY_M,
        BuildingKind::Other if footprint_m2 >= BARN_FROM_M2 => BARN_WALL_M,
        BuildingKind::Other => storeys * STOREY_M,
        _ => return None,
    })
}

/// The area of a footprint, in square metres.
pub fn footprint_area(ring: &[(f32, f32)]) -> f32 {
    twice_area(ring).abs() / 2.0
}

/// Roof pitches when the map gives no roof height: a Dutch tiled roof is
/// steep, a hip gentler.
const GABLE_PITCH_DEG: f32 = 40.0;
const HIP_PITCH_DEG: f32 = 28.0;
/// Bounds on a guessed roof height, in metres: no attic too low to read as a
/// roof, no barn roof towering over the street.
const ROOF_MIN_M: f32 = 1.2;
const ROOF_MAX_M: f32 = 6.5;
/// The lowest a wall is drawn under its roof.
const WALL_MIN_M: f32 = 2.5;
/// Houses up to this wall height keep a pitched roof by default; taller
/// buildings are blocks with flat roofs.
const PITCHED_UP_TO_M: f32 = 9.5;
/// A footprint must fill this much of its bounding rectangle to take a
/// pitched roof on that rectangle; an L or a courtyard gets a flat one.
const RECTANGULAR_ENOUGH: f32 = 0.82;
/// Eaves stand this far out from the walls.
const OVERHANG_M: f32 = 0.3;

/// Default roof colours. Dutch roofs are mostly anthracite or dark red tile;
/// Turkish small houses terracotta (the colour 18 000 Turkish roofs are
/// tagged with); flat roofs grey.
const ANTHRACITE: [f32; 3] = [0.25, 0.25, 0.27];
const DARK_TILE: [f32; 3] = [0.43, 0.21, 0.16];
const BLACK_GLAZE: [f32; 3] = [0.16, 0.16, 0.17];
const TERRACOTTA: [f32; 3] = [0.63, 0.39, 0.31];
pub const FLAT_ROOF_RGB: [f32; 3] = [0.50, 0.49, 0.48];

/// The rectangle a footprint fits best: its centre, the unit direction of its
/// long side, and its half-length and half-width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub centre: (f32, f32),
    pub along: (f32, f32),
    pub half_len: f32,
    pub half_wid: f32,
}

impl Rect {
    /// The point `u` along and `v` across, from the centre.
    pub fn at(&self, u: f32, v: f32) -> (f32, f32) {
        let (ax, az) = self.along;
        // Across is along turned a quarter: (−az, ax).
        (
            self.centre.0 + ax * u - az * v,
            self.centre.1 + az * u + ax * v,
        )
    }
}

/// Twice the signed area of a ring.
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

/// The smallest rectangle around `ring` lined up with one of its edges, and
/// how much of it the footprint fills. Buildings are drawn square to their
/// own walls, so trying each wall's direction finds the right one; a minimal
/// rectangle always has a side along the hull, and a building's hull edges
/// are its walls.
pub fn fit_rect(ring: &[(f32, f32)]) -> Option<(Rect, f32)> {
    let n = ring.len();
    if n < 3 {
        return None;
    }
    let area = twice_area(ring).abs() / 2.0;
    if !area.is_finite() || area < 1.0 {
        return None;
    }
    let mut best: Option<(f32, Rect)> = None;
    for i in 0..n {
        let (ax, az) = ring[i];
        let (bx, bz) = ring[(i + 1) % n];
        let len = ((bx - ax).powi(2) + (bz - az).powi(2)).sqrt();
        if len < 0.5 {
            continue;
        }
        let (ux, uz) = ((bx - ax) / len, (bz - az) / len);
        let (mut u_lo, mut u_hi, mut v_lo, mut v_hi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for &(x, z) in ring {
            let u = x * ux + z * uz;
            let v = -x * uz + z * ux;
            u_lo = u_lo.min(u);
            u_hi = u_hi.max(u);
            v_lo = v_lo.min(v);
            v_hi = v_hi.max(v);
        }
        let rect_area = (u_hi - u_lo) * (v_hi - v_lo);
        if best.as_ref().is_some_and(|(b, _)| *b <= rect_area) {
            continue;
        }
        let (cu, cv) = ((u_lo + u_hi) / 2.0, (v_lo + v_hi) / 2.0);
        let centre = (cu * ux - cv * uz, cu * uz + cv * ux);
        let (half_u, half_v) = ((u_hi - u_lo) / 2.0, (v_hi - v_lo) / 2.0);
        // The long side is "along".
        let rect = if half_u >= half_v {
            Rect {
                centre,
                along: (ux, uz),
                half_len: half_u,
                half_wid: half_v,
            }
        } else {
            Rect {
                centre,
                along: (-uz, ux),
                half_len: half_v,
                half_wid: half_u,
            }
        };
        best = Some((rect_area, rect));
    }
    let (rect_area, rect) = best?;
    Some((rect, (area / rect_area).min(1.0)))
}

/// How a building is roofed: the shape, the wall height up to the eaves, the
/// roof's own height above them, and its colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoofPlan {
    pub shape: RoofShape,
    pub wall_m: f32,
    pub roof_m: f32,
    pub colour: [f32; 3],
    /// The rectangle a pitched roof sits on; `None` for a flat roof.
    pub rect: Option<Rect>,
}

/// The building a roof is planned for.
#[derive(Debug, Clone, Copy)]
pub struct Planned<'a> {
    pub id: u64,
    pub outline: &'a [(f32, f32)],
    pub kind: BuildingKind,
    /// The drawn height: tagged (roof included) or worked out from storeys or
    /// type (walls only).
    pub height_m: f32,
    pub height_tagged: bool,
    pub roof: RoofTags,
}

/// The shape an untagged building's roof takes in `region`.
fn default_shape(kind: BuildingKind, wall_m: f32, region: Region) -> RoofShape {
    let low = wall_m <= PITCHED_UP_TO_M;
    match (region, kind) {
        (_, BuildingKind::Apartments | BuildingKind::Commercial) => RoofShape::Flat,
        (Region::Netherlands, BuildingKind::House | BuildingKind::Farm | BuildingKind::Shed)
            if low =>
        {
            RoofShape::Gabled
        }
        // An untagged Dutch "yes" that is house-sized is, in the Achterhoek, a house.
        (Region::Netherlands, BuildingKind::Other) if wall_m <= 7.0 => RoofShape::Gabled,
        (Region::Turkey, BuildingKind::House | BuildingKind::Other) if wall_m <= 6.5 => {
            RoofShape::Hipped
        }
        (Region::Elsewhere, BuildingKind::House | BuildingKind::Farm | BuildingKind::Shed)
            if low =>
        {
            RoofShape::Gabled
        }
        _ => RoofShape::Flat,
    }
}

/// The colour an untagged roof takes, varied per building by its id.
fn default_colour(id: u64, shape: RoofShape, region: Region) -> [f32; 3] {
    if shape == RoofShape::Flat {
        return FLAT_ROOF_RGB;
    }
    let pick = splitmix(id ^ 0x5EED) % 10;
    match region {
        Region::Netherlands => match pick {
            0..=5 => ANTHRACITE,
            6..=8 => DARK_TILE,
            _ => BLACK_GLAZE,
        },
        Region::Turkey => TERRACOTTA,
        Region::Elsewhere => {
            if pick < 6 {
                DARK_TILE
            } else {
                ANTHRACITE
            }
        }
    }
}

/// Plan a building's roof: a tag wins; otherwise what the building is and
/// where it stands decide. A pitched roof needs a footprint near enough to a
/// rectangle to sit on; anything else is roofed flat.
pub fn plan(b: Planned, region: Region) -> RoofPlan {
    let fit = fit_rect(b.outline);
    // A storey-based height is the walls; a tagged one includes the roof.
    let walls_guess = if b.height_tagged {
        (b.height_m - b.roof.height_m.unwrap_or(0.0)).max(WALL_MIN_M)
    } else {
        b.height_m
    };
    let mut shape = b
        .roof
        .shape
        .unwrap_or_else(|| default_shape(b.kind, walls_guess, region));
    let rect = match fit {
        Some((rect, fill)) if fill >= RECTANGULAR_ENOUGH => Some(rect),
        _ => None,
    };
    if rect.is_none() {
        shape = RoofShape::Flat;
    }
    let colour = b
        .roof
        .colour
        .unwrap_or_else(|| default_colour(b.id, shape, region));
    if shape == RoofShape::Flat {
        return RoofPlan {
            shape,
            wall_m: b.height_m.max(WALL_MIN_M),
            roof_m: 0.0,
            colour,
            rect: None,
        };
    }
    let rect = rect.expect("pitched roofs have a rectangle");
    let pitch = match shape {
        RoofShape::Gabled => GABLE_PITCH_DEG,
        _ => HIP_PITCH_DEG,
    };
    let guessed = (rect.half_wid * pitch.to_radians().tan()).clamp(ROOF_MIN_M, ROOF_MAX_M);
    let mut roof_m = b.roof.height_m.unwrap_or(guessed).max(0.0);
    let wall_m = if b.height_tagged {
        // The tagged height is the ridge: the walls are what is left, and a
        // roof taller than that leaves a minimum wall.
        let walls = (b.height_m - roof_m).max(WALL_MIN_M);
        roof_m = (b.height_m - walls).max(0.0);
        walls
    } else {
        b.height_m.max(WALL_MIN_M)
    };
    RoofPlan {
        shape: if roof_m > 0.1 { shape } else { RoofShape::Flat },
        wall_m,
        roof_m,
        colour,
        rect: Some(rect),
    }
}

/// The roof for `plan` with its eaves at `eave_y`, and for a gabled roof the
/// two gable walls under it in `wall` colour. Flat roofs are not drawn here.
pub fn roof_mesh(plan: &RoofPlan, eave_y: f32, wall: [f32; 4]) -> Mesh {
    let mut mesh = Mesh::default();
    let Some(rect) = plan.rect else {
        return mesh;
    };
    if plan.shape == RoofShape::Flat {
        return mesh;
    }
    let roof = [
        plan.colour[0],
        plan.colour[1],
        plan.colour[2],
        MATERIAL_PLAIN,
    ];
    let (l, w) = (rect.half_len, rect.half_wid);
    let top = eave_y + plan.roof_m;
    // The eaves stand out from the walls, and drop to keep the roof's pitch.
    let drop = plan.roof_m * OVERHANG_M / w.max(0.1);
    let (lo, wo) = (l + OVERHANG_M, w + OVERHANG_M);
    let eave = eave_y - drop;
    let p = |u: f32, v: f32, y: f32| {
        let (x, z) = rect.at(u, v);
        [x, y, z]
    };
    let face = |mesh: &mut Mesh, pts: &[[f32; 3]], colour: [f32; 4]| {
        let base = mesh.vertices.len() as u32;
        for &pos in pts {
            mesh.vertices.push(Vertex {
                pos,
                colour,
                uv: NO_UV,
            });
        }
        for k in 1..pts.len() as u32 - 1 {
            mesh.indices
                .extend_from_slice(&[base, base + k, base + k + 1]);
        }
    };
    match plan.shape {
        RoofShape::Gabled => {
            // Two slopes from the eaves to a ridge along the long side.
            for side in [-1.0f32, 1.0] {
                face(
                    &mut mesh,
                    &[
                        p(-lo, side * wo, eave),
                        p(lo, side * wo, eave),
                        p(lo, 0.0, top),
                        p(-lo, 0.0, top),
                    ],
                    roof,
                );
            }
            // Gable walls fill each end up to the ridge.
            for end in [-l, l] {
                face(
                    &mut mesh,
                    &[p(end, -w, eave_y), p(end, w, eave_y), p(end, 0.0, top)],
                    wall,
                );
            }
        }
        RoofShape::Hipped | RoofShape::Pyramidal => {
            // A hip slopes in from the ends as from the sides; where the
            // building is no longer than it is wide, the ridge is a point.
            let ridge = if plan.shape == RoofShape::Pyramidal {
                0.0
            } else {
                (l - w).max(0.0)
            };
            for side in [-1.0f32, 1.0] {
                face(
                    &mut mesh,
                    &[
                        p(-lo, side * wo, eave),
                        p(lo, side * wo, eave),
                        p(ridge, 0.0, top),
                        p(-ridge, 0.0, top),
                    ],
                    roof,
                );
                face(
                    &mut mesh,
                    &[
                        p(side * lo, -wo, eave),
                        p(side * lo, wo, eave),
                        p(side * ridge, 0.0, top),
                    ],
                    roof,
                );
            }
        }
        RoofShape::Flat => {}
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `len` × `wid` rectangle centred on the origin, its long side turned
    /// `deg` degrees from east.
    fn rect_ring(len: f32, wid: f32, deg: f32) -> Vec<(f32, f32)> {
        let (s, c) = deg.to_radians().sin_cos();
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .iter()
            .map(|&(a, b): &(f32, f32)| {
                let (u, v) = (a * len / 2.0, b * wid / 2.0);
                (u * c - v * s, u * s + v * c)
            })
            .collect()
    }

    fn building<'a>(
        outline: &'a [(f32, f32)],
        kind: BuildingKind,
        h: f32,
        tagged: bool,
    ) -> Planned<'a> {
        Planned {
            id: 42,
            outline,
            kind,
            height_m: h,
            height_tagged: tagged,
            roof: RoofTags::default(),
        }
    }

    // ── Region ────────────────────────────────────────────────────────────────

    #[test]
    fn should_place_the_riders_routes_in_their_countries() {
        assert_eq!(Region::at(51.884, 6.374), Region::Netherlands); // Oude IJsselstreek
        assert_eq!(Region::at(40.945, 29.142), Region::Turkey); // Maltepe
        assert_eq!(Region::at(43.73, 7.42), Region::Elsewhere); // Monaco
    }

    // ── Parsing ───────────────────────────────────────────────────────────────

    #[test]
    fn should_read_roof_shapes_to_what_can_be_drawn() {
        assert_eq!(RoofShape::from_tag("gabled"), Some(RoofShape::Gabled));
        assert_eq!(RoofShape::from_tag("gambrel"), Some(RoofShape::Gabled));
        assert_eq!(RoofShape::from_tag("half-hipped"), Some(RoofShape::Hipped));
        assert_eq!(RoofShape::from_tag("pyramidal"), Some(RoofShape::Pyramidal));
        assert_eq!(RoofShape::from_tag("dome"), Some(RoofShape::Flat));
        assert_eq!(RoofShape::from_tag("banana"), None);
    }

    #[test]
    fn should_read_colours_as_mappers_write_them() {
        assert_eq!(parse_colour("#ff0000"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_colour("#F00"), Some([1.0, 0.0, 0.0]));
        let a1 = parse_colour("#a1634f").expect("hex");
        assert!((a1[0] - 161.0 / 255.0).abs() < 1e-6);
        assert!(parse_colour("anthracite").is_some());
        assert_eq!(parse_colour("#12345"), None);
        assert_eq!(parse_colour("#gg0000"), None);
        assert_eq!(parse_colour("sort of reddish"), None);
    }

    // ── Fitting ───────────────────────────────────────────────────────────────

    #[test]
    fn should_fit_a_turned_rectangle_exactly_with_its_long_side_along() {
        let ring = rect_ring(12.0, 8.0, 30.0);
        let (rect, fill) = fit_rect(&ring).expect("a rectangle");
        assert!((rect.half_len - 6.0).abs() < 1e-3 && (rect.half_wid - 4.0).abs() < 1e-3);
        assert!((fill - 1.0).abs() < 1e-3);
        let angle = rect
            .along
            .1
            .atan2(rect.along.0)
            .to_degrees()
            .rem_euclid(180.0);
        assert!((angle - 30.0).abs() < 0.1, "{angle}");
    }

    #[test]
    fn should_find_an_l_shape_only_partly_fills_its_rectangle() {
        let l = [
            (0.0, 0.0),
            (20.0, 0.0),
            (20.0, 10.0),
            (10.0, 10.0),
            (10.0, 20.0),
            (0.0, 20.0),
        ];
        let (_, fill) = fit_rect(&l).expect("a rectangle");
        assert!((fill - 0.75).abs() < 1e-3, "{fill}");
    }

    #[test]
    fn should_fit_nothing_to_a_sliver_or_a_line() {
        assert!(fit_rect(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]).is_none());
        assert!(fit_rect(&[(0.0, 0.0), (1.0, 1.0)]).is_none());
    }

    // ── Plans ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_gable_a_dutch_house_and_hip_a_turkish_one() {
        let ring = rect_ring(10.0, 8.0, 0.0);
        let nl = plan(
            building(&ring, BuildingKind::House, 6.0, false),
            Region::Netherlands,
        );
        assert_eq!(nl.shape, RoofShape::Gabled);
        let tr = plan(
            building(&ring, BuildingKind::House, 6.0, false),
            Region::Turkey,
        );
        assert_eq!((tr.shape, tr.colour), (RoofShape::Hipped, TERRACOTTA));
    }

    #[test]
    fn should_roof_blocks_flat_everywhere() {
        let ring = rect_ring(30.0, 15.0, 0.0);
        for region in [Region::Netherlands, Region::Turkey, Region::Elsewhere] {
            let p = plan(
                building(&ring, BuildingKind::Apartments, 18.0, false),
                region,
            );
            assert_eq!(p.shape, RoofShape::Flat, "{region:?}");
            // A tall untagged block is flat even when it is not called apartments.
            let q = plan(building(&ring, BuildingKind::Other, 15.0, false), region);
            assert_eq!(q.shape, RoofShape::Flat, "{region:?}");
        }
    }

    #[test]
    fn should_let_a_tag_win_over_the_rules() {
        let ring = rect_ring(30.0, 15.0, 0.0);
        let b = Planned {
            roof: RoofTags {
                shape: Some(RoofShape::Gabled),
                height_m: Some(4.0),
                colour: Some([1.0, 0.0, 0.0]),
            },
            ..building(&ring, BuildingKind::Apartments, 18.0, false)
        };
        let p = plan(b, Region::Turkey);
        assert_eq!(
            (p.shape, p.roof_m, p.colour),
            (RoofShape::Gabled, 4.0, [1.0, 0.0, 0.0])
        );
    }

    #[test]
    fn should_take_the_roof_out_of_a_tagged_height() {
        // height=10 with roof:height=4: the ridge stands at 10, the walls at 6.
        let ring = rect_ring(10.0, 8.0, 0.0);
        let b = Planned {
            roof: RoofTags {
                height_m: Some(4.0),
                ..Default::default()
            },
            ..building(&ring, BuildingKind::House, 10.0, true)
        };
        let p = plan(b, Region::Netherlands);
        assert_eq!((p.wall_m, p.roof_m), (6.0, 4.0));
        assert_eq!(p.wall_m + p.roof_m, 10.0, "the tagged height is the ridge");
    }

    #[test]
    fn should_put_a_guessed_roof_on_top_of_storey_walls() {
        // Two storeys, untagged: walls 6 m, the roof above them.
        let ring = rect_ring(10.0, 8.0, 0.0);
        let p = plan(
            building(&ring, BuildingKind::House, 6.0, false),
            Region::Netherlands,
        );
        assert_eq!(p.wall_m, 6.0);
        let expected = (4.0 * GABLE_PITCH_DEG.to_radians().tan()).clamp(ROOF_MIN_M, ROOF_MAX_M);
        assert!((p.roof_m - expected).abs() < 1e-4, "{}", p.roof_m);
    }

    #[test]
    fn should_never_leave_a_wall_below_its_minimum() {
        let ring = rect_ring(10.0, 8.0, 0.0);
        let b = Planned {
            roof: RoofTags {
                height_m: Some(9.0),
                ..Default::default()
            },
            ..building(&ring, BuildingKind::House, 4.0, true)
        };
        let p = plan(b, Region::Netherlands);
        assert_eq!(p.wall_m, WALL_MIN_M);
        assert!(
            (p.wall_m + p.roof_m - 4.0).abs() < 1e-4,
            "still the tagged 4 m"
        );
    }

    #[test]
    fn should_roof_an_l_shaped_house_flat_rather_than_wrongly() {
        let l = [
            (0.0, 0.0),
            (20.0, 0.0),
            (20.0, 10.0),
            (10.0, 10.0),
            (10.0, 20.0),
            (0.0, 20.0),
        ];
        let p = plan(
            building(&l, BuildingKind::House, 6.0, false),
            Region::Netherlands,
        );
        assert_eq!(p.shape, RoofShape::Flat);
    }

    #[test]
    fn should_vary_dutch_roof_colours_but_keep_each_house_its_own() {
        let ring = rect_ring(10.0, 8.0, 0.0);
        let colours: std::collections::BTreeSet<[u32; 3]> = (0..60)
            .map(|id| {
                let p = plan(
                    Planned {
                        id,
                        ..building(&ring, BuildingKind::House, 6.0, false)
                    },
                    Region::Netherlands,
                );
                p.colour.map(|c| (c * 100.0) as u32)
            })
            .collect();
        assert!(colours.len() >= 2, "{colours:?}");
        let again = |id| {
            plan(
                Planned {
                    id,
                    ..building(&ring, BuildingKind::House, 6.0, false)
                },
                Region::Netherlands,
            )
            .colour
        };
        assert_eq!(again(7), again(7));
    }

    // ── Regional heights ──────────────────────────────────────────────────────

    #[test]
    fn should_make_an_untagged_dutch_building_a_house_or_a_barn() {
        for id in 0..100 {
            let house = regional_walls(id, BuildingKind::Other, Region::Netherlands, 90.0)
                .expect("a Dutch guess");
            assert!([3.0, 6.0, 9.0].contains(&house), "{house}");
            // House-height walls take a gable by the Dutch rule.
            assert!(house <= 9.0);
            let barn = regional_walls(id, BuildingKind::Other, Region::Netherlands, 600.0);
            assert_eq!(barn, Some(BARN_WALL_M));
        }
    }

    #[test]
    fn should_make_most_dutch_houses_two_storeys() {
        let two = (0..400)
            .filter(|&id| {
                regional_walls(id, BuildingKind::House, Region::Netherlands, 90.0) == Some(6.0)
            })
            .count();
        assert!((150..=250).contains(&two), "{two} of 400");
    }

    #[test]
    fn should_leave_other_regions_and_kinds_to_the_generic_guess() {
        assert_eq!(
            regional_walls(1, BuildingKind::Other, Region::Turkey, 90.0),
            None
        );
        assert_eq!(
            regional_walls(1, BuildingKind::House, Region::Elsewhere, 90.0),
            None
        );
        assert_eq!(
            regional_walls(1, BuildingKind::Apartments, Region::Netherlands, 90.0),
            None
        );
    }

    #[test]
    fn should_measure_a_footprint() {
        assert!((footprint_area(&rect_ring(10.0, 8.0, 37.0)) - 80.0).abs() < 1e-2);
    }

    // ── Geometry ──────────────────────────────────────────────────────────────

    #[test]
    fn should_raise_a_gable_ridge_along_the_long_side_at_the_roof_height() {
        let ring = rect_ring(12.0, 8.0, 0.0); // long side east–west
        let p = plan(
            building(&ring, BuildingKind::House, 6.0, false),
            Region::Netherlands,
        );
        let mesh = roof_mesh(&p, 6.0, [1.0; 4]);
        let top = mesh
            .vertices
            .iter()
            .map(|v| v.pos[1])
            .fold(f32::MIN, f32::max);
        assert!((top - (6.0 + p.roof_m)).abs() < 1e-4);
        // Every ridge vertex lies on the long axis (z = 0), spanning x.
        let ridge: Vec<[f32; 3]> = mesh
            .vertices
            .iter()
            .filter(|v| (v.pos[1] - top).abs() < 1e-4)
            .map(|v| v.pos)
            .collect();
        assert!(ridge.iter().all(|p| p[2].abs() < 1e-4), "{ridge:?}");
        assert!(ridge.iter().any(|p| p[0] > 5.9) && ridge.iter().any(|p| p[0] < -5.9));
        // Two slopes and two gable walls.
        assert_eq!(mesh.indices.len() / 3, 2 * 2 + 2);
    }

    #[test]
    fn should_pull_a_hip_ridge_in_from_the_ends() {
        let ring = rect_ring(12.0, 8.0, 0.0);
        let p = RoofPlan {
            shape: RoofShape::Hipped,
            ..plan(
                building(&ring, BuildingKind::House, 6.0, false),
                Region::Turkey,
            )
        };
        let mesh = roof_mesh(&p, 6.0, [1.0; 4]);
        let top = 6.0 + p.roof_m;
        let ridge_x: Vec<f32> = mesh
            .vertices
            .iter()
            .filter(|v| (v.pos[1] - top).abs() < 1e-4)
            .map(|v| v.pos[0].abs())
            .collect();
        // Half-length 6 less half-width 4: the ridge ends 2 m from the centre.
        assert!(
            ridge_x.iter().all(|&x| (x - 2.0).abs() < 1e-3),
            "{ridge_x:?}"
        );
    }

    #[test]
    fn should_draw_nothing_for_a_flat_roof() {
        let ring = rect_ring(30.0, 15.0, 0.0);
        let p = plan(
            building(&ring, BuildingKind::Apartments, 18.0, false),
            Region::Turkey,
        );
        assert!(roof_mesh(&p, 18.0, [1.0; 4]).vertices.is_empty());
    }
}
