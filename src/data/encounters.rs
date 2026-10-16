//! Encounters: short animal moments by the road, one biome's at a time
//! (the stylized-route skill, §7.9) — street dogs chasing the rider by the
//! Mediterranean, a cat crossing the street in town, a snake across the
//! desert road, deer leaping over it in the forest, a flock with its dog and
//! shepherd crossing far ahead on the farm, gulls flying alongside on the
//! coast, an eagle taking a rat in the mountains.
//!
//! Where each happens is seeded by the route (rule 6): [`triggers`] lays
//! them out once. When one starts and where its animals are each frame is
//! the [`Director`]'s, from the rider's distance and speed and the clock.
//!
//! The safety rules every encounter keeps:
//! - Anything that crosses the road has crossed it before the rider is
//!   within [`CROSSING_CLEAR_M`] of it, however fast the rider closes —
//!   the animals hurry, they never wait in the road.
//! - Dogs run on the verge, never on the track; birds fly well above the
//!   rider's head; the eagle hunts well off the road.
//! - Nothing here reaches the ride: positions only (rule 5).
//!
//! Pure: no GTK, no GL. Scene space as in `course3d`: metres, x east, z south.

use crate::data::biome::{self, Biome};
use crate::data::course3d::{position_at, RoadPoint};
use crate::data::palette::{self, Rgb};
use crate::data::props::{Motion, Prop, Shape};
use crate::data::roadway::TRACK_HALF_WIDTH_M;
use crate::data::world::{beside, splitmix};

/// A crossing is over before the rider comes within this of it.
pub const CROSSING_CLEAR_M: f32 = 18.0;
/// The flock is slower and bigger: it keeps further off.
const FLOCK_CLEAR_M: f32 = 35.0;

/// Encounters are this far apart along the route, at least and at most.
const EVERY_MIN_M: f32 = 450.0;
const EVERY_MAX_M: f32 = 1_100.0;
/// None in the first stretch: the rider has only just set off.
const FIRST_M: f32 = 300.0;

/// What happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Street dogs run out and chase the rider along the verge.
    Dogs,
    /// A cat or two cross the street ahead.
    Cats,
    /// A snake slides across the road ahead.
    Snake,
    /// Deer leap across the road ahead.
    Deer,
    /// A flock crosses far ahead, a dog working it and a shepherd behind.
    Flock,
    /// Gulls fly alongside the rider for a while.
    Gulls,
    /// An eagle circles, dives on a rat in a field and carries it off.
    Eagle,
}

impl Kind {
    /// The encounter a biome has, if any.
    pub fn of(biome: Biome) -> Option<Kind> {
        match biome {
            Biome::Mediterranean => Some(Kind::Dogs),
            Biome::Urban => Some(Kind::Cats),
            Biome::Desert => Some(Kind::Snake),
            Biome::Forest => Some(Kind::Deer),
            Biome::Farmland => Some(Kind::Flock),
            Biome::Coast => Some(Kind::Gulls),
            Biome::Mountain => Some(Kind::Eagle),
            Biome::Temperate | Biome::Polder | Biome::Wetland => None,
        }
    }

    /// A crossing: the animals go over the road, ahead of the rider.
    fn crossing(self) -> Option<Crossing> {
        let c = |seconds, lead_m, half_m, clear_m| {
            Some(Crossing {
                seconds,
                lead_m,
                half_m,
                clear_m,
            })
        };
        match self {
            Kind::Cats => c(3.0, 30.0, 10.0, CROSSING_CLEAR_M),
            Kind::Snake => c(4.0, 30.0, 6.0, CROSSING_CLEAR_M),
            Kind::Deer => c(2.4, 30.0, 18.0, CROSSING_CLEAR_M),
            Kind::Flock => c(12.0, 110.0, 28.0, FLOCK_CLEAR_M),
            Kind::Dogs | Kind::Gulls | Kind::Eagle => None,
        }
    }
}

/// How a crossing goes: it takes `seconds` at its own pace, starts when the
/// rider is `lead_m` off (further, the faster they ride), runs from
/// `half_m` one side of the road to `half_m` the other, and is over before
/// the rider is within `clear_m`.
#[derive(Debug, Clone, Copy)]
struct Crossing {
    seconds: f32,
    lead_m: f32,
    half_m: f32,
    clear_m: f32,
}

impl Crossing {
    /// How far off the rider is when it starts, at `speed_ms`: far enough
    /// that at that speed the crossing is over with room to spare.
    fn start_gap(&self, speed_ms: f32) -> f32 {
        self.lead_m
            .max(speed_ms.max(0.0) * self.seconds + self.clear_m + 6.0)
    }
}

/// One encounter's place along the route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trigger {
    /// Metres along the route: where the crossing is, where the dogs come
    /// out, the field the eagle dives on.
    pub at_m: f32,
    pub kind: Kind,
    /// Which side of the road it starts from: right of travel positive.
    pub side: f32,
    pub seed: u64,
}

/// Where the encounters are along `road`: one every 450–1100 m, of the kind
/// the biome at that point has — `biome_at(x, z)` says which. The eagle
/// hunts over open ground, on whichever side `open(x, z)` finds some; with
/// none either side, it does not hunt there.
pub fn triggers(
    road: &[RoadPoint],
    biome_at: &dyn Fn(f32, f32) -> Biome,
    open: &dyn Fn(f32, f32) -> bool,
    seed: u64,
) -> Vec<Trigger> {
    let mut out = Vec::new();
    let (Some(first), Some(last)) = (road.first(), road.last()) else {
        return out;
    };
    let mut along = first.distance_m + FIRST_M;
    let mut k = 0u64;
    while along < last.distance_m - 100.0 {
        let h = splitmix(seed ^ 0xE4C0 ^ k);
        k += 1;
        let here = along;
        along += EVERY_MIN_M + (EVERY_MAX_M - EVERY_MIN_M) * biome::unit(h);
        let Some(p) = position_at(road, here) else {
            continue;
        };
        let Some(kind) = Kind::of(biome_at(p[0], p[2])) else {
            continue;
        };
        let first_side = if h & 4 == 0 { 1.0 } else { -1.0 };
        let side = if kind == Kind::Eagle {
            // The whole field the rat runs in, and the dive's foot.
            let clear = |side: f32| {
                [-4.0f32, 0.0, 4.0].iter().all(|&da| {
                    [-3.0f32, 0.0, 3.0].iter().all(|&dl| {
                        beside(road, here + da, side * EAGLE_FIELD_M + dl)
                            .is_some_and(|(x, z)| open(x, z))
                    })
                })
            };
            match [first_side, -first_side].into_iter().find(|&s| clear(s)) {
                Some(s) => s,
                None => continue,
            }
        } else {
            first_side
        };
        out.push(Trigger {
            at_m: here,
            kind,
            side,
            seed: h,
        });
    }
    out
}

/// An encounter under way.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Active {
    trigger: Trigger,
    started_s: f32,
    /// The rider's distance, and the gap to the trigger, when it started.
    start_m: f32,
    start_gap: f32,
    /// When the dogs gave up the chase: the time, and how far along they
    /// stopped.
    gave_up: Option<(f32, f32)>,
    /// When a crossing was over.
    crossed_s: Option<f32>,
}

/// Runs the encounters as the rider rides: at most one at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct Director {
    triggers: Vec<Trigger>,
    next: usize,
    active: Option<Active>,
    last_m: Option<f32>,
}

impl Director {
    pub fn new(triggers: Vec<Trigger>) -> Self {
        Self {
            triggers,
            next: 0,
            active: None,
            last_m: None,
        }
    }

    #[cfg(test)]
    pub fn triggers(&self) -> &[Trigger] {
        &self.triggers
    }

    /// The animals to draw now, the rider `rider_m` along `road` at
    /// `speed_ms`, the clock at `now_s`.
    pub fn update(
        &mut self,
        road: &[RoadPoint],
        rider_m: f32,
        speed_ms: f32,
        now_s: f32,
    ) -> Vec<Prop> {
        if !(rider_m.is_finite() && now_s.is_finite()) {
            return Vec::new();
        }
        let speed = if speed_ms.is_finite() {
            speed_ms.max(0.0)
        } else {
            0.0
        };
        // A jump along the route — back, or far ahead — ends whatever was
        // happening and finds the next encounter from there.
        if let Some(last) = self.last_m {
            if rider_m < last - 5.0 || rider_m > last + 300.0 {
                self.active = None;
                self.next = self.triggers.partition_point(|t| t.at_m <= rider_m);
            }
        }
        self.last_m = Some(rider_m);
        if self.active.is_none() {
            self.start_next(rider_m, speed, now_s);
        }
        let Some(mut active) = self.active else {
            return Vec::new();
        };
        let (props, over) = actors(&mut active, road, rider_m, speed, now_s);
        self.active = (!over).then_some(active);
        props
    }

    fn start_next(&mut self, rider_m: f32, speed: f32, now_s: f32) {
        while let Some(t) = self.triggers.get(self.next).copied() {
            let gap = t.at_m - rider_m;
            let (start, too_late) = match t.kind.crossing() {
                Some(c) => (c.start_gap(speed), c.clear_m + 15.0),
                None => match t.kind {
                    Kind::Dogs => (55.0, 20.0),
                    Kind::Gulls => (20.0, -30.0),
                    _ => (95.0, 35.0),
                },
            };
            if gap < too_late {
                // Ridden past before it could happen: skip it.
                self.next += 1;
                continue;
            }
            if gap <= start {
                self.next += 1;
                self.active = Some(Active {
                    trigger: t,
                    started_s: now_s,
                    start_m: rider_m,
                    start_gap: gap,
                    gave_up: None,
                    crossed_s: None,
                });
            }
            return;
        }
    }
}

/// A place by the road: metres along it and metres to its right, and how
/// far above the ground there.
#[derive(Debug, Clone, Copy)]
struct Spot {
    along: f32,
    lateral: f32,
    lift: f32,
}

fn spot(along: f32, lateral: f32, lift: f32) -> Spot {
    Spot {
        along,
        lateral,
        lift,
    }
}

/// `spot` in scene space: on the track and verge at the road's height, the
/// ground falling a little lower further out.
fn place(road: &[RoadPoint], s: Spot) -> Option<[f32; 3]> {
    let (x, z) = beside(road, s.along, s.lateral)?;
    let y = position_at(road, s.along)?[1];
    let off = s.lateral.abs();
    let ground = y - 0.02 - 0.28 * ((off - 4.0) / 26.0).clamp(0.0, 1.0);
    Some([x, ground + s.lift, z])
}

fn colour(name: &str) -> Rgb {
    palette::BASE
        .named(name)
        .unwrap_or(palette::BASE.animal_dark)
}

/// One animal of an encounter: where it is now and a moment ago, so it
/// faces the way it is going.
struct Actor {
    shape: Shape,
    now: Spot,
    before: Spot,
    size: f32,
    colour: Rgb,
    motion: Motion,
    phase: f32,
}

fn to_prop(road: &[RoadPoint], a: &Actor) -> Option<Prop> {
    let p = place(road, a.now)?;
    let q = place(road, a.before)?;
    let (dx, dz) = (p[0] - q[0], p[2] - q[2]);
    let yaw = if dx.hypot(dz) > 1e-3 {
        dz.atan2(dx)
    } else {
        // Standing still: face along the road.
        let ahead = place(road, spot(a.now.along + 1.0, a.now.lateral, 0.0))?;
        (ahead[2] - p[2]).atan2(ahead[0] - p[0])
    };
    Some(Prop {
        shape: a.shape,
        pos: p,
        size: a.size,
        yaw,
        colour: a.colour,
        motion: a.motion,
        phase: a.phase,
    })
}

/// A moment ago, in seconds, for working out which way an animal faces.
const DT: f32 = 0.1;

/// The animals of `a` now, and whether it is over.
fn actors(
    a: &mut Active,
    road: &[RoadPoint],
    rider_m: f32,
    speed: f32,
    now_s: f32,
) -> (Vec<Prop>, bool) {
    let t = a.trigger;
    let elapsed = (now_s - a.started_s).max(0.0);
    let mut out = Vec::new();
    let over = match t.kind.crossing() {
        Some(c) => crossing(a, &c, rider_m, speed, now_s, &mut out),
        None => match t.kind {
            Kind::Dogs => dogs(a, rider_m, speed, now_s, &mut out),
            Kind::Gulls => gulls(&t, rider_m, speed, elapsed, &mut out),
            _ => eagle(&t, elapsed, &mut out),
        },
    };
    let props = out
        .iter()
        .filter_map(|actor| to_prop(road, actor))
        .collect();
    (props, over)
}

/// How far over the road a crossing is, 0–1: its own pace, or faster if
/// the rider is closing — 1 when they are `clear_m` off.
fn crossing_progress(a: &Active, c: &Crossing, rider_m: f32, elapsed: f32) -> f32 {
    let gap = a.trigger.at_m - rider_m;
    let by_time = elapsed / c.seconds;
    let room = (a.start_gap - c.clear_m).max(1e-3);
    let by_rider = 1.0 - (gap - c.clear_m) / room;
    by_time.max(by_rider).clamp(0.0, 1.0)
}

/// A crossing's animals. Each member follows the leader a little behind in
/// its own progress, so all are over when the leader is.
fn crossing(
    a: &mut Active,
    c: &Crossing,
    rider_m: f32,
    speed: f32,
    now_s: f32,
    out: &mut Vec<Actor>,
) -> bool {
    let t = a.trigger;
    let elapsed = (now_s - a.started_s).max(0.0);
    let p_now = crossing_progress(a, c, rider_m, elapsed);
    let p_then = crossing_progress(a, c, rider_m - speed * DT, (elapsed - DT).max(0.0));
    if p_now >= 1.0 && a.crossed_s.is_none() {
        a.crossed_s = Some(now_s);
    }
    // Once over, they walk on off the road for a moment, then are gone.
    let after = a.crossed_s.map_or(0.0, |s| now_s - s);
    if after > 3.0 {
        return true;
    }
    let members = members(&t);
    let lag_total = members.iter().map(|m| m.lag).fold(0.0, f32::max);
    for m in &members {
        let at = |p: f32, after: f32| {
            let own = (p * (1.0 + lag_total) - m.lag).clamp(0.0, 1.0);
            let lateral = t.side * (-c.half_m + 2.0 * c.half_m * own + after * 2.0) + m.across;
            // A deer clears the track in a leap.
            let lift = if t.kind == Kind::Deer && lateral.abs() < 5.0 {
                1.3 * (1.0 - (lateral / 5.0).powi(2))
            } else {
                0.0
            };
            spot(t.at_m + m.along, lateral, lift)
        };
        out.push(Actor {
            shape: m.shape,
            now: at(p_now, after),
            before: at(p_then, (after - DT).max(0.0)),
            size: m.size,
            colour: m.colour,
            motion: m.motion,
            phase: m.phase,
        });
    }
    false
}

/// One member of a crossing group: what it is, where in the group, how far
/// behind the leader.
struct Member {
    shape: Shape,
    along: f32,
    across: f32,
    lag: f32,
    size: f32,
    colour: Rgb,
    motion: Motion,
    phase: f32,
}

fn members(t: &Trigger) -> Vec<Member> {
    let u = |k: u64| biome::unit(splitmix(t.seed ^ k));
    let member = |shape, along, across, lag, size, colour, motion, k: u64| Member {
        shape,
        along,
        across,
        lag,
        size,
        colour,
        motion,
        phase: u(k + 900) * std::f32::consts::TAU,
    };
    match t.kind {
        Kind::Cats => {
            let n = 1 + (t.seed % 2);
            (0..n)
                .map(|k| {
                    let coat = ["cat_a", "animal_dark", "patch", "cat_a"]
                        [(splitmix(t.seed ^ k) % 4) as usize];
                    member(
                        Shape::Cat,
                        k as f32 * 1.5,
                        0.0,
                        k as f32 * 0.15,
                        1.5,
                        colour(coat),
                        Motion::Run,
                        k,
                    )
                })
                .collect()
        }
        Kind::Snake => vec![member(
            Shape::Snake,
            0.0,
            0.0,
            0.0,
            4.2,
            colour("snake"),
            Motion::Slither,
            0,
        )],
        Kind::Deer => {
            let n = 1 + (t.seed % 3);
            (0..n)
                .map(|k| {
                    member(
                        Shape::Deer,
                        k as f32 * 3.0,
                        0.0,
                        k as f32 * 0.12,
                        2.8,
                        colour("deer"),
                        Motion::Run,
                        k,
                    )
                })
                .collect()
        }
        Kind::Flock => {
            let fleece = ["fleece_a", "fleece_b", "fleece_c"];
            let mut out: Vec<Member> = (0..14u64)
                .map(|k| {
                    member(
                        Shape::Sheep,
                        (u(k) - 0.5) * 14.0,
                        (u(k + 50) - 0.5) * 4.0,
                        0.25 * u(k + 100),
                        2.2,
                        colour(fleece[(k % 3) as usize]),
                        Motion::Run,
                        k,
                    )
                })
                .collect();
            // The dog works the back of the flock, the shepherd follows.
            out.push(member(
                Shape::Dog,
                8.0,
                0.0,
                0.32,
                1.3,
                colour("animal_dark"),
                Motion::Run,
                200,
            ));
            out.push(member(
                Shape::Person,
                0.0,
                0.0,
                0.42,
                1.8,
                colour("coat"),
                Motion::Run,
                201,
            ));
            out
        }
        _ => Vec::new(),
    }
}

/// Street dogs: out from the side of the road at the trigger, then running
/// beside the rider on the verge, slightly behind; after a while they give
/// up, stop and fall behind.
fn dogs(a: &mut Active, rider_m: f32, speed: f32, now_s: f32, out: &mut Vec<Actor>) -> bool {
    let t = a.trigger;
    let u = |k: u64| biome::unit(splitmix(t.seed ^ k));
    let elapsed = (now_s - a.started_s).max(0.0);
    let chase_m = 110.0 + 120.0 * u(1);
    if a.gave_up.is_none() && (rider_m - a.start_m > chase_m || elapsed > 30.0) {
        a.gave_up = Some((now_s, rider_m - 2.0));
    }
    if let Some((_, stop_m)) = a.gave_up {
        if rider_m - stop_m > 150.0 {
            return true;
        }
    }
    let n = 1 + (t.seed % 3);
    let coats = ["dog_a", "dog_b", "dog_c", "animal_dark"];
    for k in 0..n {
        let kf = k as f32;
        let at = |rider: f32, time: f32| {
            let since = (time - a.started_s).max(0.0);
            let wob = (time * 2.3 + kf * 1.7).sin();
            let start = spot(t.at_m + 4.0 * kf, t.side * (16.0 + 10.0 * u(k + 10)), 0.0);
            let chase_along = match a.gave_up {
                // Slowing to a stop where they gave up.
                Some((g_s, g_m)) => g_m - 1.3 * kf + 4.0 * (1.0 - (-(time - g_s).max(0.0)).exp()),
                // Level with the rider, a little ahead: the camera is
                // behind them.
                None => rider + 1.0 - 1.3 * kf + 0.4 * wob,
            };
            let lateral = t.side * (TRACK_HALF_WIDTH_M + 1.0 + 0.6 * kf + 0.15 * wob);
            let a_in = smoothstep(since / 2.2);
            spot(
                start.along + (chase_along - start.along) * a_in,
                start.lateral + (lateral - start.lateral) * a_in,
                0.0,
            )
        };
        out.push(Actor {
            shape: Shape::Dog,
            now: at(rider_m, now_s),
            before: at(rider_m - speed * DT, now_s - DT),
            size: 1.3,
            colour: colour(coats[(splitmix(t.seed ^ (k + 20)) % 4) as usize]),
            motion: if a.gave_up.is_some_and(|(g, _)| now_s - g > 2.0) {
                Motion::Still
            } else {
                Motion::Run
            },
            phase: kf * 0.9,
        });
    }
    false
}

/// Gulls: in from the sea side, then gliding beside and above the rider a
/// while, then away up and out.
fn gulls(t: &Trigger, rider_m: f32, speed: f32, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    const STAY_S: f32 = 16.0;
    if elapsed > STAY_S + 4.0 {
        return true;
    }
    for k in 0..3u64 {
        let kf = k as f32;
        let at = |rider: f32, time: f32| {
            let come = smoothstep(time / 2.5);
            let go = smoothstep((time - STAY_S) / 4.0);
            let along = rider + 4.0 + 3.0 * kf + 1.5 * (time * 0.7 + kf).sin() + 25.0 * go;
            let near = t.side * (2.0 + 1.5 * kf + 1.2 * (time * 0.5 + kf * 2.0).sin());
            let lateral = near + t.side * (30.0 * (1.0 - come) + 40.0 * go);
            let lift =
                5.5 + kf * 0.8 + 1.0 * (time * 0.9 + kf).sin() + 20.0 * (1.0 - come) + 30.0 * go;
            spot(along, lateral, lift)
        };
        out.push(Actor {
            shape: Shape::Bird,
            now: at(rider_m, elapsed),
            before: at(rider_m - speed * DT, (elapsed - DT).max(0.0)),
            size: 1.4,
            colour: colour("gull"),
            motion: Motion::Flap,
            phase: kf * 1.3,
        });
    }
    false
}

/// The eagle: circling high over a field beside the road, diving on the
/// rat scurrying there, and climbing away with it.
fn eagle(t: &Trigger, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    const CIRCLE_S: f32 = 5.0;
    const DIVE_S: f32 = 1.6;
    const AWAY_S: f32 = 8.0;
    let caught = CIRCLE_S + DIVE_S + 0.4;
    if elapsed > caught + AWAY_S {
        return true;
    }
    let field = (t.at_m, t.side * EAGLE_FIELD_M);
    let eagle_at = |time: f32| {
        let a = time * 0.9;
        let circle = spot(field.0 + 14.0 * a.cos(), field.1 + 14.0 * a.sin(), 45.0);
        if time < CIRCLE_S {
            circle
        } else if time < CIRCLE_S + DIVE_S {
            let d = smoothstep((time - CIRCLE_S) / DIVE_S);
            let from = {
                let a = CIRCLE_S * 0.9;
                spot(field.0 + 14.0 * a.cos(), field.1 + 14.0 * a.sin(), 45.0)
            };
            spot(
                from.along + (field.0 - from.along) * d,
                from.lateral + (field.1 - from.lateral) * d,
                45.0 + (0.6 - 45.0) * d,
            )
        } else if time < caught {
            spot(field.0, field.1, 0.6)
        } else {
            let up = (time - caught) / AWAY_S;
            spot(
                field.0 + 30.0 * up,
                field.1 + t.side * 60.0 * up,
                0.6 + 55.0 * up,
            )
        }
    };
    let rat_at = |time: f32| {
        if time < caught {
            let a = time * 2.2;
            spot(field.0 + 2.5 * a.cos(), field.1 + 2.5 * a.sin(), 0.0)
        } else {
            let e = eagle_at(time);
            spot(e.along, e.lateral, e.lift - 0.5)
        }
    };
    out.push(Actor {
        shape: Shape::Bird,
        now: eagle_at(elapsed),
        before: eagle_at((elapsed - DT).max(0.0)),
        size: 3.4,
        colour: colour("eagle"),
        motion: Motion::Flap,
        phase: 0.0,
    });
    out.push(Actor {
        shape: Shape::Rat,
        now: rat_at(elapsed),
        before: rat_at((elapsed - DT).max(0.0)),
        size: 0.55,
        colour: colour("rat"),
        motion: if elapsed < caught {
            Motion::Run
        } else {
            Motion::Still
        },
        phase: 0.0,
    });
    false
}

/// The eagle's field is this far from the road's line.
pub const EAGLE_FIELD_M: f32 = 24.0;

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat road due north, `points` every 10 m: scene x is right of
    /// travel, so an animal's lateral offset is its x.
    fn north_road(points: usize) -> Vec<RoadPoint> {
        (0..points)
            .map(|i| RoadPoint {
                pos: [0.0, 10.0, -(i as f32) * 10.0],
                distance_m: i as f32 * 10.0,
            })
            .collect()
    }

    fn one(kind: Kind, at_m: f32, side: f32, seed: u64) -> Director {
        Director::new(vec![Trigger {
            at_m,
            kind,
            side,
            seed,
        }])
    }

    /// Ride `road` with speeds from `speed(t)`, 30 frames a second, calling
    /// `check(rider_m, props)` every frame; how many frames had animals.
    fn ride(
        director: &mut Director,
        road: &[RoadPoint],
        from_m: f32,
        to_m: f32,
        speed: &dyn Fn(f32, f32) -> f32,
        check: &mut dyn FnMut(f32, &[Prop]),
    ) -> usize {
        let (mut m, mut t, mut seen) = (from_m, 0.0f32, 0);
        while m < to_m && t < 600.0 {
            let v = speed(t, m);
            let props = director.update(road, m, v, t);
            if !props.is_empty() {
                seen += 1;
            }
            check(m, &props);
            m += v / 30.0;
            t += 1.0 / 30.0;
        }
        seen
    }

    /// The rider's place on the north road.
    fn rider(m: f32) -> [f32; 3] {
        [crate::data::course3d::RIDER_OFFSET_M, 10.0, -m]
    }

    fn apart(p: &Prop, q: [f32; 3]) -> f32 {
        ((p.pos[0] - q[0]).powi(2) + (p.pos[1] - q[1]).powi(2) + (p.pos[2] - q[2]).powi(2)).sqrt()
    }

    #[test]
    fn should_give_each_biome_its_own_encounter_and_the_countryside_none() {
        assert_eq!(Kind::of(Biome::Mediterranean), Some(Kind::Dogs));
        assert_eq!(Kind::of(Biome::Urban), Some(Kind::Cats));
        assert_eq!(Kind::of(Biome::Desert), Some(Kind::Snake));
        assert_eq!(Kind::of(Biome::Forest), Some(Kind::Deer));
        assert_eq!(Kind::of(Biome::Farmland), Some(Kind::Flock));
        assert_eq!(Kind::of(Biome::Coast), Some(Kind::Gulls));
        assert_eq!(Kind::of(Biome::Mountain), Some(Kind::Eagle));
        assert_eq!(Kind::of(Biome::Temperate), None);
        let road = north_road(2_001);
        assert!(triggers(&road, &|_, _| Biome::Temperate, &|_, _| true, 1).is_empty());
    }

    #[test]
    fn should_lay_encounters_450_to_1100_m_apart_the_same_every_time() {
        let road = north_road(4_001);
        let ts = triggers(&road, &|_, _| Biome::Forest, &|_, _| true, 7);
        assert!(ts.len() >= 30, "{} in 40 km", ts.len());
        assert!(ts[0].at_m >= FIRST_M);
        for w in ts.windows(2) {
            let gap = w[1].at_m - w[0].at_m;
            assert!((EVERY_MIN_M..=EVERY_MAX_M).contains(&gap), "{gap}");
        }
        assert_eq!(ts, triggers(&road, &|_, _| Biome::Forest, &|_, _| true, 7));
        assert_ne!(ts, triggers(&road, &|_, _| Biome::Forest, &|_, _| true, 8));
    }

    /// Speeds a rider might ride: steady, sprinting late, stopping in the
    /// road 40 m short of the encounter, flying down a descent.
    fn speeds() -> Vec<Box<dyn Fn(f32, f32) -> f32>> {
        vec![
            Box::new(|_, _| 8.0),
            Box::new(|t, _| if t < 3.0 { 5.0 } else { 22.0 }),
            Box::new(|t, _| if t < 40.0 { 9.0 } else { 0.0 }),
            Box::new(|_, _| 25.0),
            Box::new(|t, _| 6.0 + 6.0 * (t * 1.7).sin().abs()),
        ]
    }

    #[test]
    fn should_have_every_crossing_over_before_the_rider_reaches_it() {
        let road = north_road(201);
        for kind in [Kind::Cats, Kind::Snake, Kind::Deer, Kind::Flock] {
            // Crawling up to it, then sprinting the moment it begins: the
            // crossing must hurry, not stand in the rider's way.
            let begins = 900.0 - kind.crossing().expect("a crossing").start_gap(4.0);
            let mut all = speeds();
            all.push(Box::new(
                move |_, m| if m < begins + 0.2 { 4.0 } else { 25.0 },
            ));
            for (s, speed) in all.iter().enumerate() {
                for seed in 0..6u64 {
                    let side = if seed % 2 == 0 { 1.0 } else { -1.0 };
                    let mut d = one(kind, 900.0, side, seed);
                    let mut on_road = 0;
                    let seen = ride(
                        &mut d,
                        &road,
                        500.0,
                        1_000.0,
                        speed.as_ref(),
                        &mut |m, props| {
                            for p in props {
                                if p.pos[0].abs() < TRACK_HALF_WIDTH_M + 0.5 {
                                    on_road += 1;
                                    let gap = 900.0 - m;
                                    assert!(
                                    gap >= CROSSING_CLEAR_M - 0.5,
                                    "{kind:?} speeds {s} seed {seed}: on the road with the rider {gap} m off"
                                );
                                }
                                assert!(apart(p, rider(m)) > 4.0, "{kind:?} at the rider: {p:?}");
                            }
                        },
                    );
                    // A rider who stops short still sees it.
                    assert!(seen > 0, "{kind:?} speeds {s} seed {seed} never happened");
                    assert!(on_road > 0, "{kind:?} speeds {s} seed {seed} never crossed");
                }
            }
        }
    }

    #[test]
    fn should_keep_the_dogs_on_the_verge_beside_the_rider_then_leave_them() {
        let road = north_road(301);
        for (s, speed) in speeds().iter().enumerate() {
            for seed in 0..6u64 {
                let side = if seed % 2 == 0 { 1.0 } else { -1.0 };
                let mut d = one(Kind::Dogs, 900.0, side, seed);
                let mut closest = f32::MAX;
                ride(
                    &mut d,
                    &road,
                    700.0,
                    2_500.0,
                    speed.as_ref(),
                    &mut |m, props| {
                        for p in props {
                            assert!(
                                p.pos[0].abs() >= TRACK_HALF_WIDTH_M + 0.8,
                                "speeds {s}: a dog on the track at {:?}",
                                p.pos
                            );
                            let gap = apart(p, rider(m));
                            assert!(gap >= 1.5, "a dog in the rider at {gap} m");
                            closest = closest.min((p.pos[2] + m).abs());
                        }
                    },
                );
                if s != 2 {
                    assert!(
                        closest < 6.0,
                        "speeds {s}: the dogs never caught up ({closest})"
                    );
                    assert!(d.active.is_none(), "speeds {s}: the dogs never gave up");
                }
            }
        }
    }

    #[test]
    fn should_fly_gulls_above_the_rider_and_hunt_well_off_the_road() {
        let road = north_road(301);
        let mut d = one(Kind::Gulls, 800.0, -1.0, 3);
        let seen = ride(
            &mut d,
            &road,
            700.0,
            1_400.0,
            &|_, _| 8.0,
            &mut |m, props| {
                for p in props {
                    assert!(p.pos[1] >= 10.0 + 4.0, "a gull at head height: {:?}", p.pos);
                    assert!(apart(p, rider(m)) > 3.0);
                }
            },
        );
        assert!(seen > 30 * 15, "gulls stayed {seen} frames");
        let mut d = one(Kind::Eagle, 800.0, 1.0, 3);
        let mut lowest = f32::MAX;
        ride(
            &mut d,
            &road,
            600.0,
            1_400.0,
            &|_, _| 8.0,
            &mut |_, props| {
                for p in props {
                    // On the ground or near it, well off the road; high up it
                    // may circle wider.
                    if p.pos[1] < 10.0 + 10.0 {
                        assert!(p.pos[0].abs() >= EAGLE_FIELD_M - 3.0, "{p:?} near the road");
                    }
                    if p.shape == Shape::Bird {
                        lowest = lowest.min(p.pos[1]);
                    }
                }
            },
        );
        assert!(lowest < 11.0, "the eagle never came down ({lowest})");
    }

    #[test]
    fn should_send_the_eagle_to_the_open_side_or_nowhere() {
        let road = north_road(2_001);
        let mountain = |_: f32, _: f32| Biome::Mountain;
        // Woods east of the road: every hunt is west.
        let west = triggers(&road, &mountain, &|x, _| x < 0.0, 3);
        assert!(!west.is_empty());
        assert!(west.iter().all(|t| t.side == -1.0));
        // Woods both sides: no hunts at all.
        assert!(triggers(&road, &mountain, &|_, _| false, 3).is_empty());
    }

    #[test]
    fn should_end_an_encounter_when_the_rider_jumps_back_and_find_the_next() {
        let road = north_road(301);
        let mut d = Director::new(triggers(&road, &|_, _| Biome::Coast, &|_, _| true, 5));
        let first = d.triggers()[0].at_m;
        assert!(
            !d.update(&road, first - 10.0, 8.0, 0.0).is_empty(),
            "gulls came"
        );
        assert!(
            d.update(&road, 0.0, 8.0, 0.1).is_empty(),
            "the jump back ended them"
        );
        assert!(d.active.is_none());
        assert_eq!(d.next, 0);
        // Ridden past entirely: nothing hangs on.
        assert!(d.update(&road, 2_900.0, 8.0, 0.2).is_empty());
        assert_eq!(d.next, d.triggers().len());
    }

    #[test]
    fn should_play_the_same_encounter_the_same_way_every_time() {
        let road = north_road(201);
        let play = || {
            let mut d = one(Kind::Flock, 900.0, 1.0, 9);
            let mut frames = Vec::new();
            ride(
                &mut d,
                &road,
                600.0,
                1_000.0,
                &|_, _| 7.0,
                &mut |_, props| frames.push(props.to_vec()),
            );
            frames
        };
        assert_eq!(play(), play());
    }

    #[test]
    fn should_survive_nonsense_from_the_ride() {
        let road = north_road(101);
        let mut d = one(Kind::Dogs, 500.0, 1.0, 1);
        for (m, v, t) in [
            (f32::NAN, 8.0, 0.0),
            (480.0, f32::NAN, 0.1),
            (480.0, f32::INFINITY, 0.2),
            (480.0, -5.0, 0.3),
            (480.0, 8.0, f32::NAN),
        ] {
            for p in d.update(&road, m, v, t) {
                assert!(p.pos.iter().all(|c| c.is_finite()), "{p:?}");
            }
        }
        assert!(Director::new(Vec::new())
            .update(&[], 10.0, 8.0, 0.0)
            .is_empty());
    }
}
