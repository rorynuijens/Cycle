//! Props: the many copies of a few small shapes that fill the world — trees,
//! sheep, birds.
//!
//! Every prop of one shape is the same mesh, drawn many times over
//! (instancing). Per prop the view gets where it stands, its size, which way
//! it faces, its colour, and how it moves: animals move by transforms worked
//! out on the GPU from the time alone (the stylized-route skill, §7.8), so
//! nothing here changes once built and nothing reaches the ride.
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::course3d::Mesh;

/// The shapes a prop can take. The biome data file names them in lower
/// case, words joined: `haybale`, `lamppost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// A ball of leaves on a short trunk.
    Round,
    /// Two stacked cones: a conifer.
    Cone,
    /// A tall narrow crown: a poplar.
    Poplar,
    Sheep,
    Bird,
    Olive,
    Cypress,
    Palm,
    Cactus,
    Birch,
    Reeds,
    Bush,
    Boulder,
    HayBale,
    Barn,
    House,
    Villa,
    Hut,
    Windmill,
    Lighthouse,
    LampPost,
    Boat,
    Cow,
    Deer,
    Goat,
    Camel,
    Duck,
    Heron,
    Dog,
    Cat,
    Snake,
    Rat,
    /// A shepherd with a crook.
    Person,
    Rabbit,
    Frog,
    Tumbleweed,
    Squirrel,
    Chicken,
    Dolphin,
    Marmot,
    /// A tuft of grass on the verge.
    Tuft,
    /// A clump of wild flowers on the verge.
    Flowers,
}

impl Shape {
    pub const ALL: [Shape; 42] = [
        Shape::Round,
        Shape::Cone,
        Shape::Poplar,
        Shape::Sheep,
        Shape::Bird,
        Shape::Olive,
        Shape::Cypress,
        Shape::Palm,
        Shape::Cactus,
        Shape::Birch,
        Shape::Reeds,
        Shape::Bush,
        Shape::Boulder,
        Shape::HayBale,
        Shape::Barn,
        Shape::House,
        Shape::Villa,
        Shape::Hut,
        Shape::Windmill,
        Shape::Lighthouse,
        Shape::LampPost,
        Shape::Boat,
        Shape::Cow,
        Shape::Deer,
        Shape::Goat,
        Shape::Camel,
        Shape::Duck,
        Shape::Heron,
        Shape::Dog,
        Shape::Cat,
        Shape::Snake,
        Shape::Rat,
        Shape::Person,
        Shape::Rabbit,
        Shape::Frog,
        Shape::Tumbleweed,
        Shape::Squirrel,
        Shape::Chicken,
        Shape::Dolphin,
        Shape::Marmot,
        Shape::Tuft,
        Shape::Flowers,
    ];

    /// The shape's mesh, one unit in size, standing on the origin and
    /// facing +x.
    pub fn mesh(self) -> Mesh {
        match self {
            Shape::Round | Shape::Cone | Shape::Poplar => crate::data::trees::mesh(self),
            Shape::Sheep | Shape::Bird => crate::data::animals::mesh(self),
            _ => crate::data::kit::mesh(self),
        }
    }

    /// How far from the eye this shape is drawn, if nearer than every
    /// other prop: there are thousands of verge plants, and past this each
    /// is a pixel or two.
    pub fn reach_m(self) -> Option<f32> {
        matches!(self, Shape::Tuft | Shape::Flowers).then_some(VERGE_REACH_M)
    }

    /// An animal: hidden, with the encounters, when the rider turns animals
    /// off.
    pub fn is_animal(self) -> bool {
        matches!(
            self,
            Shape::Sheep
                | Shape::Bird
                | Shape::Cow
                | Shape::Deer
                | Shape::Goat
                | Shape::Camel
                | Shape::Duck
                | Shape::Heron
                | Shape::Dog
                | Shape::Cat
                | Shape::Snake
                | Shape::Rat
                | Shape::Person
                | Shape::Rabbit
                | Shape::Frog
                | Shape::Squirrel
                | Shape::Chicken
                | Shape::Dolphin
                | Shape::Marmot
        )
    }

    /// A tree, of any kind: what the tree rules plant.
    #[cfg(test)]
    pub fn is_tree(self) -> bool {
        matches!(
            self,
            Shape::Round
                | Shape::Cone
                | Shape::Poplar
                | Shape::Olive
                | Shape::Cypress
                | Shape::Palm
                | Shape::Birch
        )
    }
}

/// How a prop moves. The numbers are what the view's shader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Still = 0,
    /// Turns slowly this way and that, head dipping to the grass.
    Graze = 1,
    /// Circles its spot high up, wings beating.
    Orbit = 2,
    /// Rides the water, rising and falling and turning a little.
    Bob = 3,
    /// Turns the sails in front of its hub — see `kit::SAIL_FROM`.
    Spin = 4,
    /// Running: legs swinging in pairs, the body bouncing.
    Run = 5,
    /// A snake's body waving side to side.
    Slither = 6,
    /// Wings beating, in place: a bird flown by an encounter.
    Flap = 7,
    /// Rolling over and over along the way it faces: tumbleweed.
    Roll = 8,
}

/// How far an orbiting prop strays from its spot, in metres — the view's
/// shader circles at this radius.
pub const ORBIT_M: f32 = 22.0;

/// Mesh vertices marked with this colour take the prop's own colour instead:
/// a tree's crown, a sheep's fleece. Negative, so no real colour can be taken
/// for it.
pub const OWN_COLOUR: [f32; 3] = [-1.0, -1.0, -1.0];

/// One prop, ready to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prop {
    pub shape: Shape,
    /// Where it stands — for an orbiting bird, the middle of its circle.
    pub pos: [f32; 3],
    /// How big: the mesh's unit is scaled to this many metres.
    pub size: f32,
    /// Turned about the vertical, in radians.
    pub yaw: f32,
    pub colour: [f32; 3],
    pub motion: Motion,
    /// Where in its motion it starts, in radians, so a flock does not move
    /// as one.
    pub phase: f32,
}

/// Props are sent to the view in square chunks this wide, so a chunk out of
/// sight is skipped whole.
pub const CHUNK_M: f32 = 500.0;

/// Verge plants are drawn this far from the eye, in metres, shrinking away
/// over the last quarter of it rather than blinking out.
pub const VERGE_REACH_M: f32 = 200.0;

/// Floats per prop as the view reads them: x, y, z, size, yaw, red, green,
/// blue, motion, phase.
pub const INSTANCE_FLOATS: usize = 10;

/// One chunk's run of props within a [`Batch`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    pub centre: (f32, f32),
    pub start: u32,
    pub count: u32,
}

/// Every prop of one shape, chunk by chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub shape: Shape,
    /// [`INSTANCE_FLOATS`] per prop, each chunk's props together.
    pub instances: Vec<f32>,
    pub chunks: Vec<Chunk>,
}

/// The props sorted for drawing: one batch per shape that has any, each prop
/// in the chunk it stands in, chunks in a fixed order.
pub fn batches(props: &[Prop]) -> Vec<Batch> {
    Shape::ALL
        .iter()
        .filter_map(|&shape| {
            let mut cells: std::collections::BTreeMap<(i64, i64), Vec<&Prop>> =
                std::collections::BTreeMap::new();
            for p in props.iter().filter(|p| p.shape == shape) {
                let key = (
                    (p.pos[0] / CHUNK_M).floor() as i64,
                    (p.pos[2] / CHUNK_M).floor() as i64,
                );
                cells.entry(key).or_default().push(p);
            }
            if cells.is_empty() {
                return None;
            }
            let mut instances = Vec::new();
            let mut chunks = Vec::new();
            for ((i, j), members) in cells {
                chunks.push(Chunk {
                    centre: ((i as f32 + 0.5) * CHUNK_M, (j as f32 + 0.5) * CHUNK_M),
                    start: (instances.len() / INSTANCE_FLOATS) as u32,
                    count: members.len() as u32,
                });
                instances.extend(self::instances(members));
            }
            Some(Batch {
                shape,
                instances,
                chunks,
            })
        })
        .collect()
}

/// `props` as the view reads them, [`INSTANCE_FLOATS`] each.
pub fn instances<'a>(props: impl IntoIterator<Item = &'a Prop>) -> Vec<f32> {
    let mut out = Vec::new();
    for p in props {
        out.extend_from_slice(&[
            p.pos[0],
            p.pos[1],
            p.pos[2],
            p.size,
            p.yaw,
            p.colour[0],
            p.colour[1],
            p.colour[2],
            p.motion as u8 as f32,
            p.phase,
        ]);
    }
    out
}

/// Whether any of `chunk` could be in sight of an eye at `eye` looking
/// along `forward` (flat, unit length) with a view `tan_half` wide either
/// side (the tangent of half the horizontal field of view): within
/// `reach`, and not wholly behind the eye or off to either side of what it
/// sees.
pub fn in_view(
    chunk: &Chunk,
    eye: [f32; 3],
    forward: (f32, f32),
    tan_half: f32,
    reach: f32,
) -> bool {
    if !in_reach(chunk, eye[0], eye[2], reach) {
        return false;
    }
    let (dx, dz) = (chunk.centre.0 - eye[0], chunk.centre.1 - eye[2]);
    let ahead = dx * forward.0 + dz * forward.1;
    let side = (dx * forward.1 - dz * forward.0).abs();
    if !(ahead.is_finite() && side.is_finite() && tan_half.is_finite()) {
        // No direction to go on: draw it rather than lose it.
        return true;
    }
    // Half the chunk's diagonal: the furthest any of it reaches past its
    // centre in any direction. Against a side of the view, that reach
    // counts across the side's slant.
    let r = CHUNK_M * std::f32::consts::FRAC_1_SQRT_2;
    let t = tan_half.max(0.0);
    ahead >= -r && side <= ahead * t + r * (1.0 + t * t).sqrt()
}

/// Whether any of `chunk` lies within `reach` of the eye at `(x, z)`.
pub fn in_reach(chunk: &Chunk, x: f32, z: f32, reach: f32) -> bool {
    let half = CHUNK_M / 2.0;
    let dx = ((x - chunk.centre.0).abs() - half).max(0.0);
    let dz = ((z - chunk.centre.1).abs() - half).max(0.0);
    dx.hypot(dz) <= reach
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop_at(shape: Shape, x: f32, z: f32) -> Prop {
        Prop {
            shape,
            pos: [x, 1.0, z],
            size: 9.0,
            yaw: 0.5,
            colour: [0.1, 0.2, 0.3],
            motion: Motion::Graze,
            phase: 2.0,
        }
    }

    #[test]
    fn should_send_every_prop_once_in_the_chunk_it_stands_in() {
        let props: Vec<Prop> = (0..400)
            .map(|k| {
                let shape = Shape::ALL[k % Shape::ALL.len()];
                let x = (k as f32 * 37.0) % 2_300.0 - 1_100.0;
                let z = (k as f32 * 53.0) % 1_700.0 - 600.0;
                prop_at(shape, x, z)
            })
            .collect();
        let batches = batches(&props);
        assert_eq!(batches.len(), Shape::ALL.len());
        let mut sent = 0;
        for b in &batches {
            assert_eq!(b.instances.len() % INSTANCE_FLOATS, 0);
            let n = (b.instances.len() / INSTANCE_FLOATS) as u32;
            let mut next = 0;
            for c in &b.chunks {
                assert_eq!(c.start, next, "runs cover the batch in order");
                next += c.count;
                for k in c.start..c.start + c.count {
                    let at = k as usize * INSTANCE_FLOATS;
                    assert!((b.instances[at] - c.centre.0).abs() <= CHUNK_M / 2.0 + 1e-3);
                    assert!((b.instances[at + 2] - c.centre.1).abs() <= CHUNK_M / 2.0 + 1e-3);
                }
            }
            assert_eq!(next, n);
            sent += n;
        }
        assert_eq!(sent as usize, props.len());
        assert_eq!(batches, super::batches(&props), "the same order every time");
    }

    #[test]
    fn should_send_each_field_where_the_shader_reads_it() {
        assert!(batches(&[]).is_empty());
        let only = batches(&[prop_at(Shape::Sheep, 0.0, 0.0)]);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].shape, Shape::Sheep);
        assert_eq!(
            &only[0].instances,
            &[0.0, 1.0, 0.0, 9.0, 0.5, 0.1, 0.2, 0.3, 1.0, 2.0]
        );
    }

    #[test]
    fn should_skip_only_chunks_wholly_behind_the_eye_or_off_to_a_side() {
        let at = |x: f32, z: f32| Chunk {
            centre: (x, z),
            start: 0,
            count: 1,
        };
        let eye = [0.0, 0.0, 0.0];
        let north = (0.0, -1.0);
        // A view 90° wide: tan 45° = 1.
        let v = |c: &Chunk, f| in_view(c, eye, f, 1.0, 1_300.0);
        assert!(v(&at(0.0, -600.0), north), "ahead");
        assert!(v(&at(0.0, 0.0), north), "standing in it");
        assert!(v(&at(600.0, -600.0), north), "on the edge of the view");
        // A corner of it reaches level with the eye: kept.
        assert!(v(&at(0.0, 353.0), north));
        assert!(!v(&at(0.0, 360.0), north), "wholly behind");
        assert!(!v(&at(0.0, -2_000.0), north), "out of reach");
        // Off to the side: even its nearest corner is outside the 45° line.
        assert!(!v(&at(1_250.0, -200.0), north), "wholly off to the right");
        assert!(v(&at(900.0, -600.0), north), "partly in view");
        // No direction to go on: draw it rather than lose it.
        assert!(v(&at(0.0, 600.0), (f32::NAN, f32::NAN)));
    }

    #[test]
    fn should_keep_a_chunk_whose_edge_is_in_reach() {
        let c = Chunk {
            centre: (250.0, 250.0),
            start: 0,
            count: 1,
        };
        assert!(in_reach(&c, 250.0, 250.0, 0.0), "standing in it");
        assert!(
            in_reach(&c, 1_500.0, 250.0, 1_000.0),
            "edge exactly at the reach"
        );
        assert!(!in_reach(&c, 1_501.0, 250.0, 1_000.0));
        assert!(!in_reach(&c, 1_300.0, 1_300.0, 1_000.0), "corner just past");
    }

    #[test]
    fn should_draw_only_the_verge_plants_near_the_eye() {
        for shape in Shape::ALL {
            let small = matches!(shape, Shape::Tuft | Shape::Flowers);
            assert_eq!(shape.reach_m(), small.then_some(VERGE_REACH_M), "{shape:?}");
        }
    }

    #[test]
    fn should_give_every_shape_a_mesh_on_the_ground_facing_forward() {
        for shape in Shape::ALL {
            let m = shape.mesh();
            assert!(!m.vertices.is_empty(), "{shape:?}");
            let n = m.vertices.len() as u32;
            assert!(m.indices.iter().all(|&i| i < n));
            assert_eq!(m.indices.len() % 3, 0);
            let lo = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MAX, f32::min);
            assert!(
                lo.abs() < 1e-4 || shape == Shape::Bird,
                "{shape:?} starts at {lo}"
            );
        }
    }
}
