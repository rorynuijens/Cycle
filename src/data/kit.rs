//! The kit: the low-poly shapes each biome dresses its land with — olive
//! trees and cypresses, palms and cacti, reeds and boulders, barns and
//! houses, a windmill and a lighthouse, and the animals of every biome
//! (the stylized-route skill, §3, §7.1 and §7.8).
//!
//! Every shape is one mesh, drawn many times as a prop (`data/props.rs`).
//! Plants and buildings are one unit tall, standing on the origin; animals
//! are one unit long, standing on the origin and facing +x, their head past
//! [`crate::data::animals::HEAD_FROM`] so a grazing one dips it. Vertices in
//! [`OWN_COLOUR`] take the prop's own colour — a cow's coat, a house's
//! walls — and every other colour comes from the base palette by name.
//!
//! Pure: no GTK, no GL.

use crate::data::animals::cuboid;
use crate::data::course3d::{Mesh, Vertex, MATERIAL_GROUND, MATERIAL_PLAIN, MATERIAL_ROCK};
use crate::data::palette::{self, Rgb};
use crate::data::props::{Shape, OWN_COLOUR};

/// A windmill's sails are every vertex in front of this, in mesh units: the
/// view's shader turns them about the hub.
pub const SAIL_FROM: f32 = 0.21;
/// The height of a windmill's hub, in mesh units.
pub const HUB_Y: f32 = 0.78;
/// A running animal's legs are every vertex below this, in mesh units: the
/// view's shader swings them.
pub const LEGS_TOP: f32 = 0.3;
/// How far a detail on a building's wall — a window, a door panel — stands
/// out of it, in mesh units. A building is drawn 7–12 m tall, so this is
/// about 10 cm: the depth buffer tells that apart from the wall out to where
/// the haze takes the house. At a millimetre or two the wall and the window
/// took turns in front, and every window in town flickered.
const STANDS_PROUD: f32 = 0.012;

/// A plain colour from the base palette.
fn c(rgb: Rgb) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], MATERIAL_PLAIN]
}

/// The prop's own colour, lit plainly.
const OWN: [f32; 4] = [OWN_COLOUR[0], OWN_COLOUR[1], OWN_COLOUR[2], MATERIAL_PLAIN];
/// The prop's own colour on rock: blocks and cracks.
const ROCK: [f32; 4] = [OWN_COLOUR[0], OWN_COLOUR[1], OWN_COLOUR[2], MATERIAL_ROCK];
/// The prop's own colour on leaves, with the facet-to-facet shimmer the
/// land and the tree crowns have.
const LEAF: [f32; 4] = [OWN_COLOUR[0], OWN_COLOUR[1], OWN_COLOUR[2], MATERIAL_GROUND];

fn push(m: &mut Mesh, pos: [f32; 3], colour: [f32; 4]) -> u32 {
    m.vertices.push(Vertex { pos, colour });
    m.vertices.len() as u32 - 1
}

/// A flat four-cornered face, `a b c d` in order round it.
fn quad(m: &mut Mesh, corners: [[f32; 3]; 4], colour: [f32; 4]) {
    let v: Vec<u32> = corners.iter().map(|&p| push(m, p, colour)).collect();
    m.indices
        .extend_from_slice(&[v[0], v[1], v[2], v[0], v[2], v[3]]);
}

fn tri(m: &mut Mesh, corners: [[f32; 3]; 3], colour: [f32; 4]) {
    for p in corners {
        let v = push(m, p, colour);
        m.indices.push(v);
    }
}

/// An upright tapered post of `sides` sides round `(x, z)`, from radius `r0`
/// at `y0` to `r1` at `y1`, closed on top.
#[allow(clippy::too_many_arguments)]
fn tube(
    x: f32,
    z: f32,
    r0: f32,
    r1: f32,
    y0: f32,
    y1: f32,
    sides: usize,
    colour: [f32; 4],
) -> Mesh {
    let mut m = Mesh::default();
    let at = |r: f32, y: f32, k: usize| {
        let a = k as f32 / sides as f32 * std::f32::consts::TAU;
        [x + r * a.cos(), y, z + r * a.sin()]
    };
    for k in 0..sides {
        let j = (k + 1) % sides;
        quad(
            &mut m,
            [at(r0, y0, k), at(r0, y0, j), at(r1, y1, j), at(r1, y1, k)],
            colour,
        );
        tri(&mut m, [at(r1, y1, k), at(r1, y1, j), [x, y1, z]], colour);
    }
    m
}

/// A cone round `(x, z)` from radius `r` at `y0` to a point at `y1`.
fn spike(x: f32, z: f32, r: f32, y0: f32, y1: f32, sides: usize, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    for k in 0..sides {
        let a0 = k as f32 / sides as f32 * std::f32::consts::TAU;
        let a1 = (k + 1) as f32 / sides as f32 * std::f32::consts::TAU;
        tri(
            &mut m,
            [
                [x + r * a0.cos(), y0, z + r * a0.sin()],
                [x + r * a1.cos(), y0, z + r * a1.sin()],
                [x, y1, z],
            ],
            colour,
        );
    }
    m
}

/// A box from `a` to `b`, `w` wide across and `h` deep: a leg, a branch, a
/// sail's spar.
fn beam(a: [f32; 3], b: [f32; 3], w: f32, h: f32, colour: [f32; 4]) -> Mesh {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
    let d = [d[0] / len, d[1] / len, d[2] / len];
    // Across: level and square to the beam; straight up a beam, along x.
    let flat = d[0].hypot(d[2]);
    let side = if flat > 1e-4 {
        [-d[2] / flat, 0.0, d[0] / flat]
    } else {
        [1.0, 0.0, 0.0]
    };
    let up = [
        side[1] * d[2] - side[2] * d[1],
        side[2] * d[0] - side[0] * d[2],
        side[0] * d[1] - side[1] * d[0],
    ];
    let corner = |p: [f32; 3], s: f32, u: f32| {
        [
            p[0] + side[0] * s * w / 2.0 + up[0] * u * h / 2.0,
            p[1] + side[1] * s * w / 2.0 + up[1] * u * h / 2.0,
            p[2] + side[2] * s * w / 2.0 + up[2] * u * h / 2.0,
        ]
    };
    let ring = |p| {
        [
            corner(p, -1.0, -1.0),
            corner(p, 1.0, -1.0),
            corner(p, 1.0, 1.0),
            corner(p, -1.0, 1.0),
        ]
    };
    let (ra, rb) = (ring(a), ring(b));
    let mut m = Mesh::default();
    for k in 0..4 {
        let j = (k + 1) % 4;
        quad(&mut m, [ra[k], ra[j], rb[j], rb[k]], colour);
    }
    quad(&mut m, [ra[3], ra[2], ra[1], ra[0]], colour);
    quad(&mut m, rb, colour);
    m
}

/// A blade of grass or a stem: a thin flat leaf from `root` to `tip`, `w`
/// wide at its foot, turned side-on to the way it leans.
fn blade(root: [f32; 3], tip: [f32; 3], w: f32, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let (dx, dz) = (tip[0] - root[0], tip[2] - root[2]);
    let lean = dx.hypot(dz);
    let (px, pz) = if lean > 1e-4 {
        (-dz / lean * w / 2.0, dx / lean * w / 2.0)
    } else {
        (w / 2.0, 0.0)
    };
    tri(
        &mut m,
        [
            [root[0] - px, root[1], root[2] - pz],
            [root[0] + px, root[1], root[2] + pz],
            tip,
        ],
        colour,
    );
    m
}

/// A flower's head: a flat eight-faced gem round `centre`, `r` across and
/// `h` deep.
fn gem(centre: [f32; 3], r: f32, h: f32, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let [x, y, z] = centre;
    let rim = [[x + r, y, z], [x, y, z + r], [x - r, y, z], [x, y, z - r]];
    for k in 0..4 {
        let (a, b) = (rim[k], rim[(k + 1) % 4]);
        tri(&mut m, [a, b, [x, y + h, z]], colour);
        tri(&mut m, [b, a, [x, y - h, z]], colour);
    }
    m
}

/// A rounded lump of rock round `centre`, stretched by `radius`, cut flat
/// where it meets the ground: more rings and sides than a crown's ball, so
/// it reads as a worn stone, not a cut gem.
fn lump(centre: [f32; 3], radius: [f32; 3], colour: [f32; 4]) -> Mesh {
    const SIDES: usize = 10;
    const RINGS: [f32; 5] = [-0.8, -0.42, 0.0, 0.42, 0.8];
    let mut m = Mesh::default();
    let at = |x: f32, y: f32, z: f32| {
        [
            centre[0] + x * radius[0],
            (centre[1] + y * radius[1]).max(0.0),
            centre[2] + z * radius[2],
        ]
    };
    let bottom = push(&mut m, at(0.0, -1.0, 0.0), colour);
    let rings: Vec<Vec<u32>> = RINGS
        .iter()
        .enumerate()
        .map(|(r, &y)| {
            let w = (1.0 - y * y).sqrt();
            let turn = r as f32 * 0.5;
            (0..SIDES)
                .map(|k| {
                    let a = (k as f32 + turn) / SIDES as f32 * std::f32::consts::TAU;
                    push(&mut m, at(w * a.cos(), y, w * a.sin()), colour)
                })
                .collect()
        })
        .collect();
    let top = push(&mut m, at(0.0, 1.0, 0.0), colour);
    for k in 0..SIDES {
        let j = (k + 1) % SIDES;
        m.indices
            .extend_from_slice(&[bottom, rings[0][j], rings[0][k]]);
        for r in 0..rings.len() - 1 {
            let (a, b) = (&rings[r], &rings[r + 1]);
            m.indices
                .extend_from_slice(&[a[k], a[j], b[k], a[j], b[j], b[k]]);
        }
        let last = &rings[rings.len() - 1];
        m.indices.extend_from_slice(&[last[k], last[j], top]);
    }
    m
}

/// A low-poly ball round `centre`, stretched by `radius`.
fn ball(centre: [f32; 3], radius: [f32; 3], colour: [f32; 4]) -> Mesh {
    crate::data::trees::ball(centre, radius, colour)
}

/// A gable roof over `x0..x1` by `z0..z1`, its ridge along x at `top`, its
/// eaves at `eave`; the gable ends in `wall`.
#[allow(clippy::too_many_arguments)]
fn gable(
    x0: f32,
    x1: f32,
    z0: f32,
    z1: f32,
    eave: f32,
    top: f32,
    roof: [f32; 4],
    wall: [f32; 4],
) -> Mesh {
    let mut m = Mesh::default();
    let zm = (z0 + z1) / 2.0;
    quad(
        &mut m,
        [[x0, eave, z0], [x1, eave, z0], [x1, top, zm], [x0, top, zm]],
        roof,
    );
    quad(
        &mut m,
        [[x0, top, zm], [x1, top, zm], [x1, eave, z1], [x0, eave, z1]],
        roof,
    );
    tri(
        &mut m,
        [[x0, eave, z0], [x0, top, zm], [x0, eave, z1]],
        wall,
    );
    tri(
        &mut m,
        [[x1, eave, z0], [x1, eave, z1], [x1, top, zm]],
        wall,
    );
    m
}

/// A four-sided pyramid roof over `x0..x1` by `z0..z1`, from `eave` to `top`.
fn hipped(x0: f32, x1: f32, z0: f32, z1: f32, eave: f32, top: f32, roof: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    let apex = [(x0 + x1) / 2.0, top, (z0 + z1) / 2.0];
    let corners = [
        [x0, eave, z0],
        [x1, eave, z0],
        [x1, eave, z1],
        [x0, eave, z1],
    ];
    for k in 0..4 {
        tri(&mut m, [corners[k], corners[(k + 1) % 4], apex], roof);
    }
    m
}

/// Four legs under a body, `w` thick, `h` long, at `x` fore and aft and `z`
/// either side.
fn legs(x: f32, z: f32, w: f32, h: f32, colour: [f32; 4]) -> Mesh {
    let mut m = Mesh::default();
    for (lx, lz) in [(-x, -z), (-x, z), (x, -z), (x, z)] {
        m.extend(cuboid(
            [lx - w / 2.0, 0.0, lz - w / 2.0],
            [lx + w / 2.0, h, lz + w / 2.0],
            colour,
        ));
    }
    m
}

/// A shape's mesh. Shapes that are not in the kit have none here.
pub fn mesh(shape: Shape) -> Mesh {
    let b = palette::BASE;
    let mut m = Mesh::default();
    match shape {
        Shape::Olive => {
            m.extend(tube(0.0, 0.0, 0.07, 0.05, 0.0, 0.42, 6, c(b.trunk)));
            m.extend(ball([0.0, 0.62, 0.0], [0.42, 0.24, 0.42], LEAF));
            m.extend(ball([0.14, 0.78, 0.08], [0.26, 0.17, 0.26], LEAF));
        }
        Shape::Cypress => {
            m.extend(tube(0.0, 0.0, 0.03, 0.03, 0.0, 0.1, 5, c(b.trunk)));
            m.extend(ball([0.0, 0.52, 0.0], [0.13, 0.45, 0.13], LEAF));
        }
        Shape::Palm => {
            // A trunk bending over as it rises, then a crown of fronds.
            let lean = |t: f32| [0.14 * t * t, 0.005 + 0.835 * t, 0.0];
            // A foot on the ground under the first, slightly tilted, segment.
            m.extend(tube(0.0, 0.0, 0.05, 0.045, 0.0, 0.03, 6, c(b.trunk)));
            for k in 0..5 {
                let (t0, t1) = (k as f32 / 5.0, (k + 1) as f32 / 5.0);
                m.extend(beam(
                    lean(t0),
                    lean(t1),
                    0.075 - 0.01 * t0,
                    0.075 - 0.01 * t0,
                    c(b.trunk),
                ));
            }
            let top = lean(1.0);
            for k in 0..7 {
                let a = k as f32 / 7.0 * std::f32::consts::TAU + 0.3;
                let (ca, sa) = (a.cos(), a.sin());
                let mid = [top[0] + ca * 0.26, top[1] + 0.05, top[2] + sa * 0.26];
                let tip = [top[0] + ca * 0.5, top[1] - 0.14, top[2] + sa * 0.5];
                let (wx, wz) = (-sa * 0.07, ca * 0.07);
                quad(
                    &mut m,
                    [
                        top,
                        [mid[0] - wx, mid[1], mid[2] - wz],
                        tip,
                        [mid[0] + wx, mid[1], mid[2] + wz],
                    ],
                    LEAF,
                );
            }
        }
        Shape::Cactus => {
            m.extend(tube(0.0, 0.0, 0.085, 0.085, 0.0, 0.93, 7, OWN));
            m.extend(spike(0.0, 0.0, 0.085, 0.93, 1.0, 7, OWN));
            m.extend(beam([0.06, 0.42, 0.0], [0.25, 0.42, 0.0], 0.1, 0.1, OWN));
            m.extend(tube(0.25, 0.0, 0.06, 0.06, 0.38, 0.7, 6, OWN));
            m.extend(spike(0.25, 0.0, 0.06, 0.7, 0.75, 6, OWN));
            m.extend(beam(
                [-0.06, 0.55, 0.0],
                [-0.21, 0.55, 0.0],
                0.09,
                0.09,
                OWN,
            ));
            m.extend(tube(-0.21, 0.0, 0.055, 0.055, 0.51, 0.8, 6, OWN));
            m.extend(spike(-0.21, 0.0, 0.055, 0.8, 0.85, 6, OWN));
        }
        Shape::Birch => {
            m.extend(tube(0.0, 0.0, 0.026, 0.02, 0.0, 0.6, 6, c(b.birch_bark)));
            for y in [0.12, 0.26, 0.41] {
                m.extend(tube(
                    0.0,
                    0.0,
                    0.028,
                    0.028,
                    y,
                    y + 0.025,
                    6,
                    c(b.animal_dark),
                ));
            }
            m.extend(ball([0.0, 0.7, 0.0], [0.2, 0.3, 0.2], LEAF));
        }
        Shape::Reeds => {
            for k in 0..9 {
                let a = k as f32 * 2.4;
                let r = 0.12 + 0.2 * ((k * 7 % 9) as f32 / 9.0);
                let (x, z) = (r * a.cos(), r * a.sin());
                let h = 0.7 + 0.3 * ((k * 5 % 9) as f32 / 9.0);
                let tip = [x * 1.4, h, z * 1.4];
                m.extend(beam([x, 0.0, z], tip, 0.035, 0.035, OWN));
                if k % 3 == 0 {
                    let head = [tip[0] * 0.97, h - 0.16, tip[2] * 0.97];
                    m.extend(beam(
                        head,
                        [tip[0], h - 0.03, tip[2]],
                        0.07,
                        0.07,
                        c(b.cattail),
                    ));
                }
            }
        }
        Shape::Bush => {
            m.extend(ball([0.0, 0.5, 0.0], [0.55, 0.5, 0.55], LEAF));
            m.extend(ball([0.32, 0.38, 0.2], [0.32, 0.3, 0.32], LEAF));
        }
        Shape::Boulder => {
            // Two or three worn lumps leaning together, sat flat on the
            // ground, rather than one sharp ball balanced on its point.
            m.extend(lump([0.0, 0.32, 0.0], [0.62, 0.62, 0.52], ROCK));
            m.extend(lump([0.42, 0.18, 0.22], [0.36, 0.42, 0.34], ROCK));
            m.extend(lump([-0.36, 0.12, -0.26], [0.32, 0.34, 0.3], ROCK));
        }
        Shape::Tuft => {
            // Eleven blades fanning out from one root, each its own length:
            // a clump, not a weed.
            const BLADES: [(f32, f32, f32); 11] = [
                (0.0, 0.95, 0.26),
                (0.57, 0.70, 0.34),
                (1.14, 0.88, 0.20),
                (1.71, 1.00, 0.14),
                (2.28, 0.66, 0.36),
                (2.85, 0.82, 0.26),
                (3.42, 0.92, 0.18),
                (3.99, 0.72, 0.32),
                (4.56, 0.86, 0.24),
                (5.13, 0.62, 0.38),
                (5.70, 0.90, 0.20),
            ];
            for (a, h, lean) in BLADES {
                let (ca, sa) = (a.cos(), a.sin());
                m.extend(blade(
                    [0.08 * ca, 0.0, 0.08 * sa],
                    [lean * ca, h, lean * sa],
                    0.16,
                    OWN,
                ));
            }
        }
        Shape::Flowers => {
            // A few stems with a head each, and blades of leaf at their feet.
            const STEMS: [(f32, f32, f32); 5] = [
                (0.3, 0.90, 0.10),
                (1.6, 0.72, 0.22),
                (2.9, 0.84, 0.18),
                (4.1, 0.62, 0.26),
                (5.3, 0.78, 0.14),
            ];
            let stem = c(b.verge_grass);
            for (a, h, out) in STEMS {
                let (ca, sa) = (a.cos(), a.sin());
                let tip = [out * ca, h, out * sa];
                m.extend(blade(
                    [0.4 * out * ca, 0.0, 0.4 * out * sa],
                    tip,
                    0.04,
                    stem,
                ));
                m.extend(blade(
                    [0.05 * ca, 0.0, 0.05 * sa],
                    [0.5 * out * ca + 0.1 * sa, 0.32, 0.5 * out * sa - 0.1 * ca],
                    0.08,
                    stem,
                ));
                m.extend(gem(tip, 0.14, 0.07, OWN));
            }
        }
        Shape::HayBale => {
            m.extend(tube(0.0, 0.0, 0.5, 0.5, 0.0, 0.8, 8, OWN));
        }
        Shape::Barn => {
            m.extend(cuboid([-0.6, 0.0, -0.4], [0.6, 0.55, 0.4], OWN));
            m.extend(gable(-0.64, 0.64, -0.47, 0.47, 0.52, 1.0, c(b.roof_c), OWN));
            let frame = 0.6 + STANDS_PROUD;
            m.extend(cuboid([0.6, 0.0, -0.2], [frame, 0.42, 0.2], c(b.patch)));
            m.extend(cuboid(
                [frame, 0.02, -0.16],
                [frame + STANDS_PROUD, 0.38, 0.16],
                c(b.animal_dark),
            ));
        }
        Shape::House => {
            m.extend(cuboid([-0.42, 0.0, -0.36], [0.42, 0.5, 0.36], OWN));
            m.extend(gable(
                -0.46,
                0.46,
                -0.42,
                0.42,
                0.48,
                0.95,
                c(b.roof_a),
                OWN,
            ));
            m.extend(cuboid(
                [0.42, 0.0, -0.09],
                [0.42 + STANDS_PROUD, 0.3, 0.09],
                c(b.animal_dark),
            ));
            for x in [-0.22, 0.18] {
                for side in [-1.0f32, 1.0] {
                    let (wall, out) = (0.36 * side, (0.36 + STANDS_PROUD) * side);
                    m.extend(cuboid(
                        [x, 0.24, wall.min(out)],
                        [x + 0.14, 0.38, wall.max(out)],
                        c(b.animal_dark),
                    ));
                }
            }
            m.extend(cuboid([-0.25, 0.7, 0.1], [-0.15, 0.98, 0.2], c(b.roof_c)));
        }
        Shape::Villa => {
            m.extend(cuboid([-0.45, 0.0, -0.45], [0.45, 0.62, 0.45], OWN));
            m.extend(hipped(-0.52, 0.52, -0.52, 0.52, 0.6, 0.86, c(b.roof_a)));
            let face = 0.45 + STANDS_PROUD;
            m.extend(cuboid([0.45, 0.0, -0.1], [face, 0.34, 0.1], c(b.boat_b)));
            for z in [-0.3, 0.15] {
                m.extend(cuboid([0.45, 0.4, z], [face, 0.52, z + 0.15], c(b.boat_b)));
            }
        }
        Shape::Hut => {
            m.extend(cuboid([-0.4, 0.0, -0.32], [0.4, 0.45, 0.32], OWN));
            m.extend(gable(
                -0.46,
                0.46,
                -0.42,
                0.42,
                0.4,
                1.0,
                c(b.animal_dark),
                OWN,
            ));
            m.extend(cuboid(
                [0.4, 0.0, -0.08],
                [0.4 + STANDS_PROUD, 0.3, 0.08],
                c(b.cattail),
            ));
            m.extend(cuboid([-0.2, 0.6, 0.12], [-0.1, 0.98, 0.22], c(b.rock)));
        }
        Shape::Windmill => {
            m.extend(tube(0.0, 0.0, 0.2, 0.13, 0.0, 0.72, 8, c(b.windmill)));
            m.extend(spike(0.0, 0.0, 0.17, 0.7, 0.86, 8, c(b.roof_c)));
            m.extend(cuboid(
                [0.1, 0.74, -0.03],
                [0.23, 0.82, 0.03],
                c(b.windmill),
            ));
            // Four sails in front of the hub, in the plane the shader turns.
            for k in 0..4 {
                let a = k as f32 * std::f32::consts::FRAC_PI_2 + 0.4;
                let (ca, sa) = (a.cos(), a.sin());
                let at = |r: f32, w: f32| [0.26, HUB_Y + r * sa + w * ca, r * ca - w * sa];
                // Thick enough that its faces stand clear of the sail it
                // carries, which lies through its middle.
                m.extend(beam(at(0.0, 0.0), at(0.5, 0.0), 0.03, 0.03, c(b.trunk)));
                quad(
                    &mut m,
                    [at(0.1, 0.0), at(0.5, 0.0), at(0.5, 0.1), at(0.1, 0.1)],
                    c(b.sail),
                );
            }
        }
        Shape::Lighthouse => {
            for k in 0..6 {
                let (y0, y1) = (k as f32 * 0.135, (k + 1) as f32 * 0.135);
                let (r0, r1) = (0.16 - 0.08 * y0, 0.16 - 0.08 * y1);
                let colour = if k % 2 == 0 { b.lighthouse } else { b.patch };
                m.extend(tube(0.0, 0.0, r0, r1, y0, y1, 8, c(colour)));
            }
            m.extend(tube(0.0, 0.0, 0.16, 0.16, 0.81, 0.83, 8, c(b.animal_dark)));
            m.extend(tube(0.0, 0.0, 0.08, 0.08, 0.83, 0.93, 8, c(b.beak)));
            m.extend(spike(0.0, 0.0, 0.11, 0.93, 1.0, 8, c(b.animal_dark)));
            m.extend(cuboid([0.12, 0.0, -0.12], [0.36, 0.16, 0.12], c(b.patch)));
            m.extend(gable(
                0.1,
                0.38,
                -0.15,
                0.15,
                0.16,
                0.24,
                c(b.lighthouse),
                c(b.patch),
            ));
        }
        Shape::LampPost => {
            m.extend(tube(0.0, 0.0, 0.03, 0.022, 0.0, 0.95, 6, c(b.lamp)));
            m.extend(beam(
                [0.0, 0.93, 0.0],
                [0.24, 0.95, 0.0],
                0.03,
                0.03,
                c(b.lamp),
            ));
            m.extend(cuboid([0.18, 0.86, -0.05], [0.3, 0.93, 0.05], c(b.cloud)));
        }
        Shape::Boat => {
            m.extend(cuboid([-0.5, 0.0, -0.18], [0.36, 0.18, 0.18], OWN));
            let (bow, s, t) = ([0.56, 0.18, 0.0], [0.36, 0.0, 0.0], 0.18);
            tri(&mut m, [[0.36, t, -0.18], [0.36, t, 0.18], bow], OWN);
            tri(&mut m, [[0.36, t, -0.18], bow, s], OWN);
            tri(&mut m, [[0.36, t, 0.18], s, bow], OWN);
            m.extend(cuboid([-0.3, 0.18, -0.12], [0.0, 0.34, 0.12], c(b.sail)));
            m.extend(beam(
                [0.12, 0.18, 0.0],
                [0.12, 0.9, 0.0],
                0.025,
                0.025,
                c(b.trunk),
            ));
            tri(
                &mut m,
                [[0.13, 0.86, 0.0], [0.13, 0.26, 0.0], [0.42, 0.26, 0.0]],
                c(b.sail),
            );
        }
        Shape::Cow => {
            m.extend(cuboid([-0.45, 0.38, -0.21], [0.38, 0.8, 0.21], OWN));
            // White patches on the flanks.
            for (x0, x1, y0, y1) in [(-0.35, -0.1, 0.5, 0.74), (0.05, 0.25, 0.45, 0.65)] {
                for (side, flank) in [(-0.21 - STANDS_PROUD, -0.21), (0.21, 0.21 + STANDS_PROUD)] {
                    m.extend(cuboid([x0, y0, side], [x1, y1, flank], c(b.patch)));
                }
            }
            m.extend(legs(0.33, 0.14, 0.1, 0.4, c(b.animal_dark)));
            m.extend(cuboid([0.47, 0.52, -0.12], [0.7, 0.8, 0.12], OWN));
            m.extend(cuboid([0.62, 0.52, -0.1], [0.72, 0.64, 0.1], c(b.patch)));
            for z in [-0.18, 0.12] {
                m.extend(cuboid([0.5, 0.78, z], [0.55, 0.86, z + 0.06], c(b.patch)));
            }
            m.extend(beam(
                [-0.45, 0.76, 0.0],
                [-0.52, 0.42, 0.0],
                0.03,
                0.03,
                c(b.animal_dark),
            ));
        }
        Shape::Deer => {
            m.extend(cuboid([-0.38, 0.5, -0.12], [0.3, 0.76, 0.12], OWN));
            m.extend(legs(0.28, 0.08, 0.06, 0.52, OWN));
            m.extend(beam([0.26, 0.7, 0.0], [0.44, 0.92, 0.0], 0.09, 0.09, OWN));
            m.extend(cuboid([0.47, 0.86, -0.06], [0.64, 0.98, 0.06], OWN));
            m.extend(cuboid(
                [0.62, 0.86, -0.03],
                [0.66, 0.92, 0.03],
                c(b.animal_dark),
            ));
            for z in [-1.0f32, 1.0] {
                let root = [0.5, 0.98, 0.04 * z];
                let tip = [0.47, 1.18, 0.14 * z];
                m.extend(beam(root, tip, 0.025, 0.025, c(b.trunk)));
                m.extend(beam(
                    [0.485, 1.08, 0.09 * z],
                    [0.56, 1.16, 0.1 * z],
                    0.02,
                    0.02,
                    c(b.trunk),
                ));
            }
            m.extend(cuboid(
                [-0.42, 0.62, -0.04],
                [-0.38, 0.72, 0.04],
                c(b.patch),
            ));
        }
        Shape::Goat => {
            m.extend(cuboid([-0.4, 0.38, -0.16], [0.36, 0.74, 0.16], OWN));
            m.extend(legs(0.29, 0.1, 0.07, 0.4, c(b.animal_dark)));
            m.extend(cuboid([0.47, 0.6, -0.09], [0.66, 0.84, 0.09], OWN));
            m.extend(cuboid([0.55, 0.52, -0.03], [0.6, 0.6, 0.03], OWN));
            for z in [-0.06, 0.06] {
                m.extend(beam(
                    [0.5, 0.84, z],
                    [0.47, 0.98, z * 1.5],
                    0.03,
                    0.03,
                    c(b.goat_b),
                ));
            }
            m.extend(beam([-0.4, 0.7, 0.0], [-0.46, 0.8, 0.0], 0.04, 0.04, OWN));
        }
        Shape::Camel => {
            m.extend(cuboid([-0.38, 0.55, -0.15], [0.3, 0.8, 0.15], OWN));
            m.extend(ball([-0.06, 0.82, 0.0], [0.2, 0.16, 0.13], OWN));
            m.extend(legs(0.27, 0.09, 0.07, 0.58, OWN));
            m.extend(beam([0.26, 0.68, 0.0], [0.44, 0.92, 0.0], 0.09, 0.09, OWN));
            m.extend(cuboid([0.47, 0.86, -0.06], [0.64, 0.97, 0.06], OWN));
            m.extend(beam(
                [-0.38, 0.76, 0.0],
                [-0.44, 0.56, 0.0],
                0.03,
                0.03,
                c(b.animal_dark),
            ));
        }
        Shape::Duck => {
            m.extend(ball([0.0, 0.22, 0.0], [0.4, 0.22, 0.24], OWN));
            m.extend(ball([0.32, 0.48, 0.0], [0.13, 0.13, 0.12], c(b.palm_leaf)));
            m.extend(cuboid([0.44, 0.44, -0.04], [0.6, 0.49, 0.04], c(b.beak)));
            m.extend(cuboid([-0.42, 0.28, -0.08], [-0.3, 0.36, 0.08], OWN));
        }
        Shape::Heron => {
            for z in [-0.05, 0.05] {
                m.extend(beam([0.0, 0.0, z], [0.0, 0.58, z], 0.025, 0.025, c(b.beak)));
            }
            m.extend(ball([-0.02, 0.68, 0.0], [0.26, 0.12, 0.12], OWN));
            m.extend(beam([0.18, 0.72, 0.0], [0.45, 0.96, 0.0], 0.05, 0.05, OWN));
            m.extend(ball([0.5, 0.98, 0.0], [0.07, 0.05, 0.05], OWN));
            m.extend(beam(
                [0.54, 0.98, 0.0],
                [0.7, 0.95, 0.0],
                0.025,
                0.025,
                c(b.beak),
            ));
            m.extend(beam(
                [-0.24, 0.68, 0.0],
                [-0.36, 0.62, 0.0],
                0.06,
                0.03,
                c(b.animal_dark),
            ));
        }
        Shape::Dog => {
            m.extend(cuboid([-0.38, 0.3, -0.11], [0.3, 0.55, 0.11], OWN));
            m.extend(legs(0.26, 0.07, 0.07, 0.32, OWN));
            m.extend(cuboid([0.3, 0.45, -0.09], [0.5, 0.66, 0.09], OWN));
            m.extend(cuboid(
                [0.5, 0.47, -0.05],
                [0.6, 0.56, 0.05],
                c(b.animal_dark),
            ));
            for z in [-0.08, 0.05] {
                m.extend(cuboid(
                    [0.33, 0.66, z],
                    [0.39, 0.74, z + 0.03],
                    c(b.animal_dark),
                ));
            }
            m.extend(beam([-0.36, 0.5, 0.0], [-0.52, 0.68, 0.0], 0.05, 0.05, OWN));
        }
        Shape::Cat => {
            m.extend(cuboid([-0.32, 0.25, -0.08], [0.25, 0.45, 0.08], OWN));
            m.extend(legs(0.22, 0.05, 0.05, 0.27, OWN));
            m.extend(cuboid([0.25, 0.36, -0.08], [0.42, 0.53, 0.08], OWN));
            for z in [-0.07, 0.04] {
                tri(
                    &mut m,
                    [[0.28, 0.53, z], [0.36, 0.53, z], [0.31, 0.62, z + 0.015]],
                    OWN,
                );
            }
            m.extend(beam(
                [-0.3, 0.42, 0.0],
                [-0.42, 0.76, 0.0],
                0.035,
                0.035,
                OWN,
            ));
        }
        Shape::Snake => {
            // A low flat body in segments, the shader waving it; dark bands.
            for k in 0..8 {
                let (x0, x1) = (-0.5 + k as f32 * 0.115, -0.5 + (k + 1) as f32 * 0.115);
                let w = 0.02 + 0.025 * (k as f32 / 7.0);
                let colour = if k % 2 == 0 { OWN } else { c(b.animal_dark) };
                m.extend(cuboid([x0, 0.0, -w], [x1, 0.045, w], colour));
            }
            m.extend(cuboid([0.42, 0.0, -0.05], [0.52, 0.055, 0.05], OWN));
        }
        Shape::Rat => {
            m.extend(ball([0.0, 0.16, 0.0], [0.24, 0.16, 0.13], OWN));
            m.extend(ball([0.25, 0.14, 0.0], [0.09, 0.08, 0.07], OWN));
            m.extend(beam(
                [-0.22, 0.12, 0.0],
                [-0.55, 0.04, 0.0],
                0.03,
                0.03,
                c(b.skin),
            ));
        }
        Shape::Person => {
            for z in [-0.06, 0.06] {
                m.extend(cuboid(
                    [-0.05, 0.0, z - 0.045],
                    [0.05, 0.46, z + 0.045],
                    c(b.kit),
                ));
            }
            m.extend(cuboid([-0.09, 0.42, -0.15], [0.09, 0.8, 0.15], OWN));
            m.extend(ball([0.0, 0.87, 0.0], [0.075, 0.08, 0.075], c(b.skin)));
            m.extend(cuboid([-0.13, 0.92, -0.13], [0.13, 0.935, 0.13], c(b.hut)));
            m.extend(cuboid([-0.07, 0.935, -0.07], [0.07, 1.0, 0.07], c(b.hut)));
            m.extend(beam(
                [0.12, 0.0, 0.2],
                [0.12, 0.98, 0.2],
                0.025,
                0.025,
                c(b.trunk),
            ));
            m.extend(beam(
                [0.12, 0.98, 0.2],
                [0.2, 0.98, 0.2],
                0.025,
                0.025,
                c(b.trunk),
            ));
        }
        Shape::Rabbit => {
            m.extend(ball([-0.05, 0.3, 0.0], [0.32, 0.22, 0.18], OWN));
            m.extend(ball([0.3, 0.44, 0.0], [0.14, 0.13, 0.12], OWN));
            for z in [-0.05, 0.05] {
                m.extend(beam([0.26, 0.54, z], [0.2, 0.88, z * 1.4], 0.06, 0.03, OWN));
            }
            m.extend(ball([-0.38, 0.36, 0.0], [0.07, 0.07, 0.07], c(b.patch)));
        }
        Shape::Frog => {
            m.extend(ball([0.0, 0.2, 0.0], [0.4, 0.2, 0.3], OWN));
            for z in [-0.12, 0.12] {
                m.extend(ball([0.24, 0.38, z], [0.07, 0.07, 0.07], c(b.patch)));
                m.extend(ball(
                    [0.28, 0.4, z],
                    [0.035, 0.035, 0.035],
                    c(b.animal_dark),
                ));
                m.extend(beam(
                    [-0.2, 0.1, z * 1.6],
                    [-0.42, 0.04, z * 2.4],
                    0.07,
                    0.05,
                    OWN,
                ));
            }
        }
        Shape::Tumbleweed => {
            // Hoops of dry twigs round a centre half a unit up: the shader
            // rolls it about that centre.
            for k in 0..6 {
                let tilt = k as f32 * std::f32::consts::PI / 6.0;
                let mut last: Option<[f32; 3]> = None;
                for s in 0..=8 {
                    let a = s as f32 / 8.0 * std::f32::consts::TAU;
                    let (r, y) = (0.45 * a.cos(), 0.45 * a.sin());
                    let p = [r * tilt.cos(), 0.5 + y, r * tilt.sin()];
                    if let Some(q) = last {
                        m.extend(beam(q, p, 0.035, 0.035, OWN));
                    }
                    last = Some(p);
                }
            }
        }
        Shape::Squirrel => {
            m.extend(cuboid([-0.25, 0.16, -0.08], [0.2, 0.36, 0.08], OWN));
            m.extend(legs(0.16, 0.05, 0.05, 0.18, OWN));
            m.extend(cuboid([0.2, 0.26, -0.07], [0.36, 0.42, 0.07], OWN));
            for z in [-0.05, 0.03] {
                m.extend(cuboid([0.24, 0.42, z], [0.28, 0.5, z + 0.02], OWN));
            }
            m.extend(ball([-0.38, 0.55, 0.0], [0.16, 0.3, 0.11], OWN));
        }
        Shape::Chicken => {
            for z in [-0.06, 0.06] {
                m.extend(beam([0.0, 0.0, z], [0.0, 0.22, z], 0.03, 0.03, c(b.beak)));
            }
            m.extend(ball([0.0, 0.4, 0.0], [0.32, 0.22, 0.2], OWN));
            m.extend(ball([0.27, 0.66, 0.0], [0.1, 0.11, 0.09], OWN));
            m.extend(cuboid(
                [0.22, 0.75, -0.02],
                [0.32, 0.82, 0.02],
                c(b.lighthouse),
            ));
            m.extend(cuboid([0.36, 0.63, -0.025], [0.44, 0.67, 0.025], c(b.beak)));
            m.extend(beam([-0.28, 0.45, 0.0], [-0.4, 0.66, 0.0], 0.1, 0.04, OWN));
        }
        Shape::Dolphin => {
            m.extend(ball([0.0, 0.2, 0.0], [0.5, 0.16, 0.14], OWN));
            m.extend(beam([0.45, 0.17, 0.0], [0.62, 0.15, 0.0], 0.05, 0.05, OWN));
            tri(
                &mut m,
                [[-0.1, 0.33, 0.0], [0.1, 0.33, 0.0], [-0.12, 0.5, 0.0]],
                OWN,
            );
            tri(
                &mut m,
                [[-0.48, 0.2, 0.0], [-0.62, 0.22, 0.16], [-0.62, 0.22, -0.16]],
                OWN,
            );
        }
        Shape::Marmot => {
            m.extend(ball([0.0, 0.3, 0.0], [0.2, 0.3, 0.18], OWN));
            m.extend(ball([0.05, 0.66, 0.0], [0.12, 0.11, 0.11], OWN));
            m.extend(cuboid(
                [0.15, 0.62, -0.03],
                [0.19, 0.67, 0.03],
                c(b.animal_dark),
            ));
            for z in [-0.08, 0.08] {
                m.extend(beam([0.12, 0.45, z], [0.2, 0.36, z], 0.05, 0.05, OWN));
            }
        }
        _ => {}
    }
    // A leaning beam's corner can dip a hair under its foot, and a body
    // with no legs can float: stand the whole shape on the ground, never in
    // it or over it.
    let lowest = m
        .vertices
        .iter()
        .map(|v| v.pos[1])
        .fold(f32::INFINITY, f32::min);
    let lowest = if lowest.is_finite() { lowest } else { 0.0 };
    for v in &mut m.vertices {
        v.pos[1] -= lowest;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::animals::HEAD_FROM;

    /// Every flat, axis-aligned face of `shape`'s mesh: which axis it faces
    /// along, where on it, its colour and its extent across the other two.
    fn flat_faces(shape: Shape) -> Vec<(usize, f32, [f32; 4], [f32; 4])> {
        let m = shape.mesh();
        let mut faces = Vec::new();
        for t in m.indices.chunks_exact(3) {
            let v = [0, 1, 2].map(|k| m.vertices[t[k] as usize]);
            for axis in 0..3 {
                let c = v[0].pos[axis];
                if v.iter().any(|p| (p.pos[axis] - c).abs() > 1e-6) {
                    continue;
                }
                let (u, w) = ((axis + 1) % 3, (axis + 2) % 3);
                let lo_u = v.iter().map(|p| p.pos[u]).fold(f32::MAX, f32::min);
                let hi_u = v.iter().map(|p| p.pos[u]).fold(f32::MIN, f32::max);
                let lo_w = v.iter().map(|p| p.pos[w]).fold(f32::MAX, f32::min);
                let hi_w = v.iter().map(|p| p.pos[w]).fold(f32::MIN, f32::max);
                faces.push((axis, c, v[0].colour, [lo_u, hi_u, lo_w, hi_w]));
            }
        }
        faces
    }

    #[test]
    fn should_stand_every_detail_a_depth_step_clear_of_the_face_behind_it() {
        // Two faces of different colours a hair apart and facing the same
        // way take turns in front with distance: windows on a house, patches
        // on a cow, all flickered. A detail stands STANDS_PROUD out, or lies
        // flush and wholly behind (the hidden back of a box).
        let mut close = Vec::new();
        for shape in Shape::ALL {
            let faces = flat_faces(shape);
            for (i, a) in faces.iter().enumerate() {
                for b in &faces[i + 1..] {
                    if a.0 != b.0 || a.2 == b.2 {
                        continue;
                    }
                    let gap = (a.1 - b.1).abs();
                    let overlap_u = a.3[1].min(b.3[1]) - a.3[0].max(b.3[0]);
                    let overlap_w = a.3[3].min(b.3[3]) - a.3[2].max(b.3[2]);
                    if gap > 1e-5
                        && gap < STANDS_PROUD - 1e-6
                        && overlap_u > 1e-4
                        && overlap_w > 1e-4
                    {
                        close.push(format!("{shape:?}: {gap} apart at {} on axis {}", a.1, a.0));
                    }
                }
            }
        }
        close.dedup();
        assert!(
            close.is_empty(),
            "faces too close to tell apart:\n{}",
            close.join("\n")
        );
    }

    #[test]
    fn should_stand_house_windows_out_of_the_wall_by_a_depth_step() {
        // Windows 1 mm proud of the wall flickered in town: the wall and the
        // window took turns in front of each other.
        let dark = c(palette::BASE.animal_dark);
        let m = mesh(Shape::House);
        let windows: Vec<f32> = m
            .vertices
            .iter()
            .filter(|v| v.colour == dark && (0.24..=0.38).contains(&v.pos[1]))
            .filter(|v| v.pos[2].abs() > 0.3)
            .map(|v| v.pos[2])
            .collect();
        let front = windows.iter().copied().fold(f32::MAX, f32::min);
        let back = windows.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            front <= -0.36 - STANDS_PROUD + 1e-6,
            "front windows out to {front}"
        );
        assert!(
            back >= 0.36 + STANDS_PROUD - 1e-6,
            "back windows out to {back}"
        );
    }

    #[test]
    fn should_stand_the_barn_door_out_of_its_frame_by_a_depth_step() {
        let m = mesh(Shape::Barn);
        let out_to = |colour: [f32; 4]| {
            m.vertices
                .iter()
                .filter(|v| v.colour == colour)
                .map(|v| v.pos[0])
                .fold(f32::MIN, f32::max)
        };
        let frame = out_to(c(palette::BASE.patch));
        let door = out_to(c(palette::BASE.animal_dark));
        assert!(
            frame >= 0.6 + STANDS_PROUD - 1e-6,
            "the frame stands out {frame}"
        );
        assert!(
            door >= frame + STANDS_PROUD - 1e-6,
            "the door stands out {door} from {frame}"
        );
    }

    #[test]
    fn should_build_a_closed_box_between_any_two_points() {
        for (a, b) in [
            ([0.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            ([0.2, 0.1, -0.3], [0.5, 0.9, 0.4]),
        ] {
            let m = beam(a, b, 0.1, 0.1, OWN);
            assert_eq!(m.indices.len(), 36, "six faces");
            // Every corner lies within half a diagonal of the segment.
            for v in &m.vertices {
                assert!(v.pos.iter().all(|p| p.is_finite()));
            }
        }
    }

    #[test]
    fn should_sit_a_boulder_on_a_flat_base_as_rounded_rock() {
        let m = mesh(Shape::Boulder);
        let on_ground = m.vertices.iter().filter(|v| v.pos[1].abs() < 1e-5).count();
        assert!(
            on_ground >= 8,
            "only {on_ground} corners on the ground: it rocks on a point"
        );
        assert!(
            m.vertices.iter().all(|v| v.pos[1] >= 0.0),
            "nothing below the ground"
        );
        assert!(
            m.indices.len() / 3 >= 120,
            "{} facets is a cut gem, not a rounded rock",
            m.indices.len() / 3
        );
        assert!(
            m.vertices.iter().all(|v| v.colour[3] == MATERIAL_ROCK),
            "drawn as rock"
        );
    }

    #[test]
    fn should_stand_every_plant_and_building_inside_its_unit() {
        for shape in [
            Shape::Olive,
            Shape::Cypress,
            Shape::Palm,
            Shape::Cactus,
            Shape::Birch,
            Shape::Reeds,
            Shape::Bush,
            Shape::Boulder,
            Shape::Tuft,
            Shape::Flowers,
            Shape::HayBale,
            Shape::Barn,
            Shape::House,
            Shape::Villa,
            Shape::Hut,
            Shape::Lighthouse,
            Shape::LampPost,
        ] {
            let m = mesh(shape);
            let top = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MIN, f32::max);
            assert!(
                (0.6..=1.0 + 1e-4).contains(&top),
                "{shape:?} tops out at {top}"
            );
        }
    }

    #[test]
    fn should_put_every_grazers_head_and_nothing_else_past_the_dip_line() {
        // The shader dips every vertex past HEAD_FROM: only a head may be
        // there, and every grazer must have one there.
        for shape in [
            Shape::Cow,
            Shape::Deer,
            Shape::Goat,
            Shape::Camel,
            Shape::Heron,
        ] {
            let m = mesh(shape);
            let head: Vec<&Vertex> = m.vertices.iter().filter(|v| v.pos[0] > HEAD_FROM).collect();
            assert!(!head.is_empty(), "{shape:?} has no head to dip");
            for v in head {
                assert!(v.pos[1] > 0.5, "{shape:?}: {:?} dips with the head", v.pos);
            }
        }
    }

    #[test]
    fn should_keep_a_windmill_hub_and_sails_in_front_of_the_line_the_shader_turns() {
        let m = mesh(Shape::Windmill);
        let sail = palette::BASE.sail;
        for v in m.vertices.iter().filter(|v| v.colour[..3] == sail) {
            assert!(v.pos[0] > SAIL_FROM, "a sail behind the line: {:?}", v.pos);
        }
        // Nothing of the tower is in front of it: it would turn with them.
        for v in m.vertices.iter().filter(|v| v.pos[0] > SAIL_FROM) {
            let r = (v.pos[1] - HUB_Y).hypot(v.pos[2]);
            assert!(r <= 0.6, "{:?} would swing round the hub", v.pos);
        }
    }
}
