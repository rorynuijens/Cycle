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

/// Further than this from the rider an animal is a speck in the haze, and
/// can go without anyone seeing it go.
pub const OUT_OF_SIGHT_M: f32 = 160.0;
/// Leaving animals walk out into the fields no further than this from the
/// road's line: the ground they stand on is the road's, carried out, and
/// further off the land's own hills would bury or strand them.
const LEAVE_REACH_M: f32 = 45.0;
/// How fast a leaving animal goes out from the road, at least, in m/s.
const LEAVE_OUT_MS: f32 = 3.0;
/// How fast a dolphin sinks out of sight, in m/s.
const LEAVE_SINK_MS: f32 = 1.5;
/// Animals leaving on foot tire, slowing along the road by this time
/// constant, in seconds, so the rider always comes past them in the end.
const LEAVE_TIRE_S: f32 = 8.0;
/// However they are leaving, they are gone after this long, in seconds:
/// only a rider who has stopped still sees them go.
const LEAVE_MAX_S: f32 = 180.0;

/// What happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Street dogs run out and chase the rider along the verge.
    Dogs,
    /// A goat stands on the verge and watches the rider go by.
    Goat,
    /// A cat or two cross the street ahead.
    Cats,
    /// Pigeons on the pavement burst up as the rider passes.
    Pigeons,
    /// A snake slides across the road ahead.
    Snake,
    /// Tumbleweed rolls across the road ahead.
    Tumbleweed,
    /// A camel caravan plods along beside the road.
    Camels,
    /// Deer leap across the road ahead.
    Deer,
    /// A squirrel dashes across the road ahead.
    Squirrel,
    /// An owl swoops low over the road ahead, from wood to wood.
    Owl,
    /// A flock crosses far ahead, a dog working it and a shepherd behind.
    Flock,
    /// Chickens on the verge scatter as the rider comes.
    Chickens,
    /// Gulls fly alongside the rider for a while.
    Gulls,
    /// Dolphins leap in the sea beside the road, keeping pace.
    Dolphins,
    /// An eagle circles, dives on a rat in a field and carries it off.
    Eagle,
    /// Marmots pop up beside the road and watch the rider go by.
    Marmots,
    /// Rabbits dart across the road ahead.
    Rabbits,
    /// A flock of sheep runs along in the field beside the road.
    FlockRun,
    /// Geese fly over the road in a V.
    Geese,
    /// A heron takes off from beside a ditch as the rider comes.
    Heron,
    /// Frogs hop across the road ahead.
    Frogs,
    /// Ducks take off from beside the road as the rider comes.
    Ducks,
}

impl Kind {
    /// Every kind.
    #[cfg(test)]
    pub const ALL: [Kind; 22] = [
        Kind::Dogs,
        Kind::Goat,
        Kind::Cats,
        Kind::Pigeons,
        Kind::Snake,
        Kind::Tumbleweed,
        Kind::Camels,
        Kind::Deer,
        Kind::Squirrel,
        Kind::Owl,
        Kind::Flock,
        Kind::Chickens,
        Kind::Gulls,
        Kind::Dolphins,
        Kind::Eagle,
        Kind::Marmots,
        Kind::Rabbits,
        Kind::FlockRun,
        Kind::Geese,
        Kind::Heron,
        Kind::Frogs,
        Kind::Ducks,
    ];

    /// The encounters a biome has: one is picked for each spot.
    pub fn choices(biome: Biome) -> &'static [Kind] {
        match biome {
            Biome::Temperate => &[Kind::Rabbits, Kind::FlockRun],
            Biome::Forest => &[Kind::Deer, Kind::Squirrel, Kind::Owl],
            Biome::Farmland => &[Kind::Flock, Kind::Chickens],
            Biome::Polder => &[Kind::Geese, Kind::Heron],
            Biome::Mediterranean => &[Kind::Dogs, Kind::Goat],
            Biome::Desert => &[Kind::Snake, Kind::Tumbleweed, Kind::Camels],
            Biome::Coast => &[Kind::Gulls, Kind::Dolphins],
            Biome::Mountain => &[Kind::Eagle, Kind::Marmots],
            Biome::Wetland => &[Kind::Frogs, Kind::Ducks],
            Biome::Urban => &[Kind::Cats, Kind::Pigeons],
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
            Kind::Rabbits => c(1.8, 25.0, 9.0, CROSSING_CLEAR_M),
            Kind::Frogs => c(4.0, 28.0, 5.0, CROSSING_CLEAR_M),
            Kind::Tumbleweed => c(4.0, 30.0, 14.0, CROSSING_CLEAR_M),
            Kind::Squirrel => c(1.6, 25.0, 9.0, CROSSING_CLEAR_M),
            _ => None,
        }
    }

    /// When an encounter that is not a crossing starts: the rider this far
    /// off its spot, and too late once they are nearer than the second.
    fn window(self) -> (f32, f32) {
        match self {
            Kind::Dogs => (55.0, 20.0),
            Kind::Gulls => (20.0, -30.0),
            Kind::Eagle => (95.0, 35.0),
            Kind::Goat | Kind::Marmots => (60.0, 5.0),
            Kind::Pigeons => (60.0, 25.0),
            Kind::Chickens => (50.0, 21.0),
            Kind::Heron | Kind::Ducks => (70.0, 31.0),
            Kind::FlockRun => (70.0, 10.0),
            Kind::Camels => (120.0, -50.0),
            Kind::Geese => (110.0, 20.0),
            Kind::Owl => (40.0, 15.0),
            Kind::Dolphins => (80.0, -40.0),
            _ => (40.0, 15.0),
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

/// Where the encounters are along `road`: one every 450–1100 m, picked
/// from the encounters the biome at that point has — `biome_at(x, z)` says
/// which. Some need the right ground: the eagle and the marmots open
/// ground, `open(x, z)`, on one side or the other; the dolphins the sea,
/// `sea(x, z)`, alongside for the stretch they swim. A spot whose pick has
/// neither tries the biome's others, then has none.
pub fn triggers(
    road: &[RoadPoint],
    biome_at: &dyn Fn(f32, f32) -> Biome,
    open: &dyn Fn(f32, f32) -> bool,
    sea: &dyn Fn(f32, f32) -> bool,
    seed: u64,
) -> Vec<Trigger> {
    let mut out = Vec::new();
    let (Some(first), Some(last)) = (road.first(), road.last()) else {
        return out;
    };
    let mut along = first.distance_m + FIRST_M;
    let mut k = 0u64;
    let mut last_kind: Option<Kind> = None;
    while along < last.distance_m - 100.0 {
        let h = splitmix(seed ^ 0xE4C0 ^ k);
        k += 1;
        let here = along;
        along += EVERY_MIN_M + (EVERY_MAX_M - EVERY_MIN_M) * biome::unit(h);
        let Some(p) = position_at(road, here) else {
            continue;
        };
        let choices = Kind::choices(biome_at(p[0], p[2]));
        let first_side = if h & 4 == 0 { 1.0 } else { -1.0 };
        // Every point of a patch, `half_along` either way and `half_across`
        // either side of `off` metres out, passes `test`.
        let patch = |side: f32,
                     off: f32,
                     half_along: f32,
                     half_across: f32,
                     test: &dyn Fn(f32, f32) -> bool| {
            [-1.0f32, 0.0, 1.0].iter().all(|&da| {
                [-1.0f32, 0.0, 1.0].iter().all(|&dl| {
                    beside(road, here + da * half_along, side * off + dl * half_across)
                        .is_some_and(|(x, z)| test(x, z))
                })
            })
        };
        let side_for = |kind: Kind| -> Option<f32> {
            let fits = |side: f32| match kind {
                Kind::Eagle => patch(side, EAGLE_FIELD_M, 4.0, 3.0, open),
                Kind::Marmots => patch(side, 12.0, 4.0, 3.0, open),
                Kind::Dolphins => (0..6).all(|j| {
                    beside(road, here + j as f32 * 50.0, side * DOLPHINS_M)
                        .is_some_and(|(x, z)| sea(x, z))
                }),
                _ => true,
            };
            [first_side, -first_side].into_iter().find(|&s| fits(s))
        };
        let n = choices.len();
        let pick = (splitmix(h ^ 0x91C) % n as u64) as usize;
        let order = || (0..n).map(|j| choices[(pick + j) % n]);
        // Not the same as last time, if the biome has another that fits.
        let found = order()
            .filter(|&kind| Some(kind) != last_kind)
            .chain(order())
            .find_map(|kind| side_for(kind).map(|side| (kind, side)));
        if let Some((kind, side)) = found {
            last_kind = Some(kind);
            out.push(Trigger {
                at_m: here,
                kind,
                side,
                seed: h,
            });
        }
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
    cue: Option<(f32, f32)>,
    /// When a crossing was over.
    crossed_s: Option<f32>,
}

/// Runs the encounters as the rider rides: at most one at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct Director {
    triggers: Vec<Trigger>,
    next: usize,
    active: Option<Active>,
    /// The active encounter's animals as last drawn: where they leave from.
    last_actors: Vec<Actor>,
    /// The animals of encounters that are over, making their way out of
    /// sight, and when they set off.
    leaving: Vec<Actor>,
    left_s: f32,
    last_m: Option<f32>,
}

impl Director {
    pub fn new(triggers: Vec<Trigger>) -> Self {
        Self {
            triggers,
            next: 0,
            active: None,
            last_actors: Vec::new(),
            leaving: Vec::new(),
            left_s: 0.0,
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
                self.last_actors.clear();
                self.leaving.clear();
                self.next = self.triggers.partition_point(|t| t.at_m <= rider_m);
            }
        }
        self.last_m = Some(rider_m);
        if self.active.is_none() {
            self.start_next(rider_m, speed, now_s);
        }
        let mut now = Vec::new();
        if let Some(mut active) = self.active {
            let over;
            (now, over) = actors(&mut active, rider_m, speed, now_s);
            if over {
                // Over, but its animals do not vanish where they stand: they
                // go on their way until nobody can see them go.
                self.active = None;
                self.leaving = std::mem::take(&mut self.last_actors);
                self.left_s = now_s;
                now.clear();
            } else {
                self.active = Some(active);
                self.last_actors.clone_from(&now);
            }
        }
        let since = (now_s - self.left_s).max(0.0);
        if since > LEAVE_MAX_S {
            self.leaving.clear();
        }
        // Once out of sight, gone for good: a flock outrunning a slow rider
        // must not run back into the picture from behind the camera.
        self.leaving
            .retain(|a| !out_of_sight(&leave(a, since), rider_m));
        now.extend(self.leaving.iter().map(|a| leave(a, since)));
        now.iter().filter_map(|a| to_prop(road, a)).collect()
    }

    fn start_next(&mut self, rider_m: f32, speed: f32, now_s: f32) {
        while let Some(t) = self.triggers.get(self.next).copied() {
            let gap = t.at_m - rider_m;
            let (start, too_late) = match t.kind.crossing() {
                Some(c) => (c.start_gap(speed), c.clear_m + 15.0),
                None => t.kind.window(),
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
                    cue: None,
                    crossed_s: None,
                });
            }
            return;
        }
    }
}

/// A place by the road: metres along it and metres to its right, and how
/// far above the ground there.
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
struct Actor {
    shape: Shape,
    now: Spot,
    before: Spot,
    /// Turned to watch this spot, whichever way it is going.
    look: Option<Spot>,
    size: f32,
    colour: Rgb,
    motion: Motion,
    phase: f32,
}

fn to_prop(road: &[RoadPoint], a: &Actor) -> Option<Prop> {
    let p = place(road, a.now)?;
    let q = place(road, a.before)?;
    let (dx, dz) = match a.look.and_then(|l| place(road, l)) {
        Some(l) => (l[0] - p[0], l[2] - p[2]),
        None => (p[0] - q[0], p[2] - q[2]),
    };
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

/// Whether nobody watching the road can see an animal at `s`, the rider
/// `rider_m` along: behind the camera, far off in the haze, or gone under
/// ground or water.
fn out_of_sight(a: &Actor, rider_m: f32) -> bool {
    let s = a.now;
    s.along < rider_m - crate::data::course3d::CAMERA_TRAIL_M - 3.0
        || (s.along - rider_m).hypot(s.lateral) > OUT_OF_SIGHT_M
        || s.lift < -0.6 * a.size
}

/// `a`, `since` seconds after its encounter ended: going on the way it was
/// going, and out from the road. Birds fly on, and whatever a bird carries;
/// animals on the ground go no further out than [`LEAVE_REACH_M`], tire,
/// and stand there; dolphins sink.
fn leave(a: &Actor, since: f32) -> Actor {
    let v = |now: f32, before: f32| (now - before) / DT;
    let (va, vl, vf) = (
        v(a.now.along, a.before.along),
        v(a.now.lateral, a.before.lateral),
        v(a.now.lift, a.before.lift),
    );
    let out = if a.now.lateral < 0.0 { -1.0 } else { 1.0 };
    let vo = (vl * out).max(LEAVE_OUT_MS);
    let flying = a.motion == Motion::Flap || a.now.lift > 2.0;
    let at = |t: f32| {
        if flying {
            return spot(
                a.now.along + va * t,
                a.now.lateral + out * vo * t,
                a.now.lift + vf.max(0.0) * t,
            );
        }
        if a.shape == Shape::Dolphin {
            return spot(
                a.now.along + va * t,
                a.now.lateral + out * vo * t,
                a.now.lift - LEAVE_SINK_MS * t,
            );
        }
        let reach = a.now.lateral.abs().max(LEAVE_REACH_M);
        spot(
            a.now.along + va * LEAVE_TIRE_S * (1.0 - (-t / LEAVE_TIRE_S).exp()),
            out * (a.now.lateral.abs() + vo * t).min(reach),
            a.now.lift,
        )
    };
    let now = at(since);
    let before = at((since - DT).max(0.0));
    // Out in the field and tired: it stands there, facing the way it went.
    let still = !flying
        && a.shape != Shape::Dolphin
        && (now.along - before.along).abs() < 0.3 * DT
        && now.lateral == before.lateral;
    if still {
        return Actor {
            now,
            before: now,
            look: Some(spot(now.along + va, now.lateral + out * vo, now.lift)),
            motion: Motion::Still,
            ..*a
        };
    }
    // On its way: one that stood or grazed gets up and goes.
    let motion = match a.motion {
        Motion::Still | Motion::Graze if !flying && a.shape != Shape::Dolphin => Motion::Run,
        m => m,
    };
    Actor {
        now,
        before,
        motion,
        ..*a
    }
}

/// The animals of `a` now, and whether it is over.
fn actors(a: &mut Active, rider_m: f32, speed: f32, now_s: f32) -> (Vec<Actor>, bool) {
    let t = a.trigger;
    let elapsed = (now_s - a.started_s).max(0.0);
    let mut out = Vec::new();
    let over = match t.kind.crossing() {
        Some(c) => crossing(a, &c, rider_m, speed, now_s, &mut out),
        None => match t.kind {
            Kind::Dogs => dogs(a, rider_m, speed, now_s, &mut out),
            Kind::Gulls => gulls(&t, rider_m, speed, elapsed, &mut out),
            Kind::Eagle => eagle(&t, elapsed, &mut out),
            Kind::FlockRun | Kind::Camels => alongside(&t, rider_m, elapsed, &mut out),
            Kind::Goat | Kind::Marmots => watchers(a, rider_m, now_s, &mut out),
            Kind::Pigeons | Kind::Chickens | Kind::Heron | Kind::Ducks => {
                take_off(a, rider_m, now_s, &mut out)
            }
            Kind::Geese => geese(&t, elapsed, &mut out),
            Kind::Owl => owl(&t, elapsed, &mut out),
            Kind::Dolphins => dolphins(&t, rider_m, speed, elapsed, &mut out),
            _ => true,
        },
    };
    (out, over)
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
            look: None,
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
        Kind::Rabbits | Kind::Squirrel => {
            let (shape, name, n) = if t.kind == Kind::Rabbits {
                (Shape::Rabbit, "rabbit", 1 + t.seed % 3)
            } else {
                (Shape::Squirrel, "squirrel", 1)
            };
            (0..n)
                .map(|k| {
                    let (along, lag) = (k as f32 * 2.0 + u(k) * 1.5, k as f32 * 0.1);
                    member(shape, along, 0.0, lag, 1.0, colour(name), Motion::Run, k)
                })
                .collect()
        }
        Kind::Frogs => (0..3 + t.seed % 3)
            .map(|k| {
                let along = (u(k) - 0.5) * 6.0;
                member(
                    Shape::Frog,
                    along,
                    0.0,
                    0.3 * u(k + 7),
                    0.7,
                    colour("frog"),
                    Motion::Run,
                    k,
                )
            })
            .collect(),
        Kind::Tumbleweed => (0..1 + t.seed % 2)
            .map(|k| {
                let size = 1.2 + 0.5 * u(k);
                member(
                    Shape::Tumbleweed,
                    k as f32 * 6.0,
                    0.0,
                    k as f32 * 0.2,
                    size,
                    colour("tumbleweed"),
                    Motion::Roll,
                    k,
                )
            })
            .collect(),
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
    if a.cue.is_none() && (rider_m - a.start_m > chase_m || elapsed > 30.0) {
        a.cue = Some((now_s, rider_m - 2.0));
    }
    if let Some((_, stop_m)) = a.cue {
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
            let chase_along = match a.cue {
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
            look: None,
            size: 1.3,
            colour: colour(coats[(splitmix(t.seed ^ (k + 20)) % 4) as usize]),
            motion: if a.cue.is_some_and(|(g, _)| now_s - g > 2.0) {
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
            look: None,
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
        look: None,
        size: 3.4,
        colour: colour("eagle"),
        motion: Motion::Flap,
        phase: 0.0,
    });
    out.push(Actor {
        shape: Shape::Rat,
        now: rat_at(elapsed),
        before: rat_at((elapsed - DT).max(0.0)),
        look: None,
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

/// The dolphins swim this far from the road's line, well out in the sea.
pub const DOLPHINS_M: f32 = 80.0;

/// A herd going the rider's way beside the road: sheep running along in the
/// field, or camels plodding in a line further out, the rider soon past.
fn alongside(t: &Trigger, rider_m: f32, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    let u = |k: u64| biome::unit(splitmix(t.seed ^ k));
    let (shape, n, pace, off, gap, size, coats, last_s): (
        _,
        u64,
        f32,
        f32,
        f32,
        f32,
        &[&str],
        f32,
    ) = if t.kind == Kind::FlockRun {
        (
            Shape::Sheep,
            8,
            5.0,
            14.0,
            2.5,
            1.9,
            &["fleece_a", "fleece_b", "fleece_c"],
            30.0,
        )
    } else {
        (Shape::Camel, 4, 1.0, 26.0, 5.0, 3.6, &["camel"], 90.0)
    };
    let lead = t.at_m + pace * elapsed;
    if elapsed > last_s || rider_m > lead + 80.0 {
        return true;
    }
    for k in 0..n {
        let kf = k as f32;
        let at = |time: f32| {
            let jig = if t.kind == Kind::FlockRun {
                1.5 * (time * 1.3 + kf).sin()
            } else {
                0.0
            };
            spot(
                t.at_m + pace * time - gap * kf + jig,
                t.side * (off + 4.0 * u(k)),
                0.0,
            )
        };
        out.push(Actor {
            shape,
            now: at(elapsed),
            before: at((elapsed - DT).max(0.0)),
            look: None,
            size,
            colour: colour(coats[(k as usize) % coats.len()]),
            motion: Motion::Run,
            phase: kf * 0.7,
        });
    }
    false
}

/// Animals that stop and watch the rider go by: a goat on the verge, or
/// marmots popping up from their burrows and ducking back once passed.
fn watchers(a: &mut Active, rider_m: f32, now_s: f32, out: &mut Vec<Actor>) -> bool {
    let t = a.trigger;
    let elapsed = (now_s - a.started_s).max(0.0);
    if elapsed > 60.0 {
        return true;
    }
    let marmots = t.kind == Kind::Marmots;
    // Passed: the marmots duck back down.
    if marmots && a.cue.is_none() && rider_m > t.at_m + 10.0 {
        a.cue = Some((now_s, rider_m));
    }
    if !marmots && rider_m > t.at_m + 40.0 {
        return true;
    }
    let down = a.cue.map_or(0.0, |(s, _)| (now_s - s) / 0.5);
    if down > 1.0 {
        return true;
    }
    let rider = spot(rider_m, crate::data::course3d::RIDER_OFFSET_M, 0.0);
    let n = if marmots { 3 } else { 1 };
    for k in 0..n {
        let kf = k as f32;
        let (here, shape, size, name, lift) = if marmots {
            let up = smoothstep(elapsed / 0.6) - smoothstep(down);
            (
                spot(
                    t.at_m + 3.0 * kf,
                    t.side * (10.0 + 2.5 * kf),
                    -0.5 * (1.0 - up),
                ),
                Shape::Marmot,
                0.8,
                "marmot",
                0.0,
            )
        } else {
            (
                spot(t.at_m, t.side * 4.8, 0.0),
                Shape::Goat,
                1.8,
                "goat_b",
                0.0,
            )
        };
        let here = spot(here.along, here.lateral, here.lift + lift);
        // The goat grazes till the rider is near, then looks up to watch.
        let grazing = !marmots && t.at_m - rider_m > 25.0;
        out.push(Actor {
            shape,
            now: here,
            before: here,
            look: (!grazing).then_some(rider),
            size,
            colour: colour(name),
            motion: if grazing {
                Motion::Graze
            } else {
                Motion::Still
            },
            phase: kf,
        });
    }
    false
}

/// Birds on the ground beside the road take off as the rider comes — the
/// pigeons, a heron, ducks — flying up and away from the road; chickens
/// scatter away from it on foot.
fn take_off(a: &mut Active, rider_m: f32, now_s: f32, out: &mut Vec<Actor>) -> bool {
    let t = a.trigger;
    let u = |k: u64| biome::unit(splitmix(t.seed ^ k));
    let (n, near, far, cue_gap, ground_shape, size, name, flier_size): (
        u64,
        f32,
        f32,
        f32,
        _,
        f32,
        &str,
        f32,
    ) = match t.kind {
        Kind::Pigeons => (8, 5.0, 9.0, 22.0, Shape::Bird, 0.6, "pigeon", 0.6),
        Kind::Chickens => (6, 4.5, 8.0, 18.0, Shape::Chicken, 0.8, "chicken", 0.8),
        Kind::Heron => (1, 16.0, 18.0, 28.0, Shape::Heron, 1.8, "heron", 2.2),
        _ => (4, 22.0, 26.0, 30.0, Shape::Duck, 0.9, "duck", 1.0),
    };
    if a.cue.is_none() && t.at_m - rider_m <= cue_gap {
        a.cue = Some((now_s, rider_m));
    }
    let since = a.cue.map(|(s, _)| now_s - s);
    if since.is_some_and(|s| s > 5.0) || now_s - a.started_s > 60.0 {
        return true;
    }
    for k in 0..n {
        let kf = k as f32;
        let home = spot(
            t.at_m + (u(k) - 0.5) * 8.0,
            t.side * (near + (far - near) * u(k + 20)),
            0.0,
        );
        let at = |s: Option<f32>| match s {
            None => home,
            // Away from the road on foot.
            Some(s) if t.kind == Kind::Chickens => spot(
                home.along + (u(k + 40) - 0.5) * 2.0 * s,
                home.lateral + t.side * 4.0 * s,
                0.0,
            ),
            // Up and away from the road, a little ahead.
            Some(s) => spot(
                home.along + 2.0 * s,
                home.lateral + t.side * (5.0 + kf * 0.5) * s,
                (2.5 + 0.3 * kf) * s + 0.6 * s * s,
            ),
        };
        let flying = since.is_some() && t.kind != Kind::Chickens;
        out.push(Actor {
            shape: if flying { Shape::Bird } else { ground_shape },
            now: at(since),
            before: at(since.map(|s| (s - DT).max(0.0))),
            look: None,
            size: if flying { flier_size } else { size },
            colour: colour(name),
            motion: match (flying, since.is_some()) {
                (true, _) => Motion::Flap,
                (false, true) => Motion::Run,
                (false, false) if t.kind == Kind::Heron || t.kind == Kind::Ducks => Motion::Still,
                _ => Motion::Graze,
            },
            phase: kf * 1.1,
        });
    }
    false
}

/// Geese in a V, flying low over the road from one side to the other.
fn geese(t: &Trigger, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    const SPEED: f32 = 9.0;
    const REACH: f32 = 90.0;
    let lead = -REACH + SPEED * elapsed;
    if lead > REACH + 20.0 {
        return true;
    }
    for k in 0..9u64 {
        let rank = k.div_ceil(2) as f32;
        let wing = if k % 2 == 0 { 1.0 } else { -1.0 };
        let at = |time: f32| {
            let l = -REACH + SPEED * time - rank * 2.5;
            spot(
                t.at_m + wing * rank * 2.5,
                t.side * l,
                15.0 + 0.4 * (time + rank).sin(),
            )
        };
        out.push(Actor {
            shape: Shape::Bird,
            now: at(elapsed),
            before: at((elapsed - DT).max(0.0)),
            look: None,
            size: 1.6,
            colour: colour("goose"),
            motion: Motion::Flap,
            phase: k as f32 * 0.4,
        });
    }
    false
}

/// An owl swooping from the wood on one side to the wood on the other, low
/// over the road but well above the rider's head.
fn owl(t: &Trigger, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    const SECONDS: f32 = 3.0;
    const HALF: f32 = 22.0;
    if elapsed > SECONDS {
        return true;
    }
    let at = |time: f32| {
        let l = -HALF + 2.0 * HALF * (time / SECONDS);
        spot(t.at_m, t.side * l, 7.5 - 2.0 * (1.0 - (l / HALF).powi(2)))
    };
    out.push(Actor {
        shape: Shape::Bird,
        now: at(elapsed),
        before: at((elapsed - DT).max(0.0)),
        look: None,
        size: 2.2,
        colour: colour("owl"),
        motion: Motion::Flap,
        phase: 0.0,
    });
    false
}

/// Dolphins keeping pace with the rider out in the sea, leaping clear of
/// the water now and then.
fn dolphins(t: &Trigger, rider_m: f32, speed: f32, elapsed: f32, out: &mut Vec<Actor>) -> bool {
    const STAY_S: f32 = 22.0;
    if elapsed > STAY_S {
        return true;
    }
    for k in 0..3u64 {
        let kf = k as f32;
        let at = |rider: f32, time: f32| {
            let leap = (time * 1.4 + kf * 1.1).sin().max(0.0);
            spot(
                // Well ahead: level with the rider they would be out of
                // the camera's view, it looks up the road.
                rider + 100.0 + 9.0 * kf + 3.0 * (time * 0.5 + kf).sin(),
                t.side * (DOLPHINS_M + 6.0 * kf),
                -2.0 + 4.0 * leap,
            )
        };
        out.push(Actor {
            shape: Shape::Dolphin,
            now: at(rider_m, elapsed),
            before: at(rider_m - speed * DT, (elapsed - DT).max(0.0)),
            look: None,
            size: 3.6,
            colour: colour("dolphin"),
            motion: Motion::Still,
            phase: kf,
        });
    }
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
    fn should_give_every_biome_two_or_three_encounters_and_use_every_kind() {
        let mut used = std::collections::HashSet::new();
        for b in Biome::ALL {
            let choices = Kind::choices(b);
            assert!((2..=3).contains(&choices.len()), "{b:?}");
            used.extend(choices.iter().copied());
        }
        assert_eq!(used.len(), Kind::ALL.len(), "a kind no biome has");
        // Over a long ride in one biome, each of its encounters turns up.
        let road = north_road(8_001);
        for b in Biome::ALL {
            let kinds: std::collections::HashSet<Kind> =
                triggers(&road, &|_, _| b, &|_, _| true, &|_, _| true, 4)
                    .iter()
                    .map(|t| t.kind)
                    .collect();
            assert_eq!(kinds.len(), Kind::choices(b).len(), "{b:?}: {kinds:?}");
        }
    }

    #[test]
    fn should_never_play_the_same_encounter_twice_running_where_there_is_a_choice() {
        let road = north_road(8_001);
        for b in Biome::ALL {
            let ts = triggers(&road, &|_, _| b, &|_, _| true, &|_, _| true, 9);
            // Short of the road's end, where the dolphins' stretch of sea
            // runs out and only the gulls fit.
            let before_end: Vec<&Trigger> = ts.iter().filter(|t| t.at_m < 79_500.0).collect();
            for w in before_end.windows(2) {
                assert_ne!(w[0].kind, w[1].kind, "{b:?} at {} m", w[1].at_m);
            }
        }
    }

    #[test]
    fn should_keep_the_dolphins_to_the_sea_and_fall_back_to_the_gulls() {
        let road = north_road(4_001);
        let coast = |_: f32, _: f32| Biome::Coast;
        let none = triggers(&road, &coast, &|_, _| true, &|_, _| false, 2);
        assert!(!none.is_empty());
        assert!(
            none.iter().all(|t| t.kind == Kind::Gulls),
            "dolphins on dry land"
        );
        let west = triggers(&road, &coast, &|_, _| true, &|x, _| x < -70.0, 2);
        let dolphins: Vec<&Trigger> = west.iter().filter(|t| t.kind == Kind::Dolphins).collect();
        assert!(!dolphins.is_empty());
        assert!(
            dolphins.iter().all(|t| t.side == -1.0),
            "dolphins on the dry side"
        );
    }

    #[test]
    fn should_keep_every_kind_off_the_road_near_the_rider_or_high_above_it() {
        // The one rule every encounter keeps: on the track, an animal is
        // either crossing well ahead of the rider or flying well above.
        let road = north_road(301);
        for kind in Kind::ALL {
            let clear = kind.crossing().map(|c| c.clear_m);
            for (s, speed) in speeds().iter().enumerate() {
                for seed in 0..4u64 {
                    let side = if seed % 2 == 0 { 1.0 } else { -1.0 };
                    let mut d = one(kind, 900.0, side, seed);
                    ride(
                        &mut d,
                        &road,
                        500.0,
                        2_000.0,
                        speed.as_ref(),
                        &mut |m, props| {
                            for p in props {
                                let r = rider(m);
                                let high = p.pos[1] >= 10.0 + 4.0;
                                if p.pos[0].abs() < TRACK_HALF_WIDTH_M + 0.5 && !high {
                                    let gap = 900.0 - m;
                                    let ok = clear.is_some_and(|c| gap >= c - 0.5);
                                    assert!(ok, "{kind:?} speeds {s} seed {seed}: on the track {gap} m from the rider: {p:?}");
                                }
                                // The dogs run alongside, a couple of metres off:
                                // their own test holds them to that.
                                let near = if kind == Kind::Dogs { 1.5 } else { 2.5 };
                                let apart = apart(p, r);
                                assert!(
                                    apart > near,
                                    "{kind:?} speeds {s}: {apart} m from the rider: {p:?}"
                                );
                            }
                        },
                    );
                }
            }
        }
    }

    #[test]
    fn should_play_every_kind_and_then_end_it() {
        let road = north_road(401);
        for kind in Kind::ALL {
            let mut d = one(kind, 900.0, 1.0, 11);
            let seen = ride(&mut d, &road, 600.0, 3_500.0, &|_, _| 8.0, &mut |_, _| {});
            assert!(seen > 10, "{kind:?} showed for {seen} frames");
            assert!(d.active.is_none(), "{kind:?} never ended");
        }
    }

    #[test]
    fn should_lay_encounters_450_to_1100_m_apart_the_same_every_time() {
        let road = north_road(4_001);
        let ts = triggers(&road, &|_, _| Biome::Forest, &|_, _| true, &|_, _| true, 7);
        assert!(ts.len() >= 30, "{} in 40 km", ts.len());
        assert!(ts[0].at_m >= FIRST_M);
        for w in ts.windows(2) {
            let gap = w[1].at_m - w[0].at_m;
            assert!((EVERY_MIN_M..=EVERY_MAX_M).contains(&gap), "{gap}");
        }
        assert_eq!(
            ts,
            triggers(&road, &|_, _| Biome::Forest, &|_, _| true, &|_, _| true, 7)
        );
        assert_ne!(
            ts,
            triggers(&road, &|_, _| Biome::Forest, &|_, _| true, &|_, _| true, 8)
        );
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
        let west = triggers(&road, &mountain, &|x, _| x < 0.0, &|_, _| true, 3);
        assert!(!west.is_empty());
        assert!(west.iter().all(|t| t.side == -1.0));
        // Woods both sides: no hunts at all.
        assert!(triggers(&road, &mountain, &|_, _| false, &|_, _| true, 3).is_empty());
    }

    #[test]
    fn should_end_an_encounter_when_the_rider_jumps_back_and_find_the_next() {
        let road = north_road(301);
        let mut d = Director::new(triggers(
            &road,
            &|_, _| Biome::Coast,
            &|_, _| true,
            &|_, _| true,
            5,
        ));
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

    #[test]
    fn should_never_end_an_encounter_while_its_animals_are_in_view() {
        // An encounter is over when its animals are out of the picture —
        // behind the camera, far off in the haze, or gone under ground or
        // water — not when a clock says so. A flock running along the field
        // used to vanish in plain view whenever the rider was too slow to
        // pass it in half a minute.
        const FAR_M: f32 = OUT_OF_SIGHT_M;
        let road = north_road(301);
        let ground = 10.0;
        let mut seen = Vec::new();
        for kind in Kind::ALL {
            for (seed, side) in [(1u64, 1.0f32), (2, -1.0), (7, 1.0)] {
                for v in [2.0f32, 4.0, 6.0, 8.0, 10.0, 13.0] {
                    let mut d = one(kind, 600.0, side, seed);
                    let mut last: Vec<Prop> = Vec::new();
                    let mut ended = None;
                    ride(&mut d, &road, 300.0, 2_800.0, &|_, _| v, &mut |m, props| {
                        if props.is_empty() && !last.is_empty() && ended.is_none() {
                            ended = Some(m);
                            let camera_m = m - crate::data::course3d::CAMERA_TRAIL_M;
                            for p in &last {
                                let along = -p.pos[2];
                                let away = apart(p, rider(m));
                                let off = p.pos[0].abs();
                                let ground_y =
                                    ground - 0.02 - 0.28 * ((off - 4.0) / 26.0).clamp(0.0, 1.0);
                                // A frame's grace: these are where the
                                // animals were a frame before they went.
                                let hidden = along < camera_m + 0.5
                                    || away > FAR_M - 2.0
                                    || p.pos[1] < ground_y - 0.6 * p.size + 0.15;
                                if !hidden {
                                    seen.push(format!(
                                        "{kind:?} seed {seed} at {v} m/s: a {:?} vanished \
                                         {away:.0} m from the rider, {:.0} m along of them",
                                        p.shape,
                                        along - m
                                    ));
                                    break;
                                }
                            }
                        }
                        last = props.to_vec();
                    });
                }
            }
        }
        assert!(seen.is_empty(), "{}", seen.join("\n"));
    }
}
