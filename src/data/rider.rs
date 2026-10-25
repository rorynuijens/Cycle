//! The rider on the road: a low-poly cyclist the camera follows.
//!
//! Built of boxes, facing +x, wheels on the ground at y = 0. The legs are
//! their own mesh, hung from the hip, so the view can turn them with the
//! cranks; everything else is one mesh. What the rider looks like is an
//! [`Avatar`]: today always the default, one day the rider's own.
//!
//! Pure: no GTK, no GL. Rider space: metres, x forward, y up, z to the right.

use crate::data::animals::cuboid;
use crate::data::course3d::{Mesh, Vertex, MATERIAL_PLAIN};
use crate::data::palette::{self, Rgb};

/// Where the legs hang from, in rider space.
pub const HIP: [f32; 3] = [-0.14, 0.94, 0.0];
/// How far either leg hangs to the side of the middle.
pub const LEG_Z: f32 = 0.11;
/// How far the bike rolls for one turn of the cranks, in metres: a gear for
/// about 90 rpm at 35 km/h.
pub const METRES_PER_TURN: f32 = 6.5;
/// How far a leg swings either way at the hip, in radians, and how much a
/// foot rises and falls with the pedal, in metres.
const SWING_RAD: f32 = 0.42;
const LIFT_M: f32 = 0.07;

const WHEEL_R: f32 = 0.34;
const WHEEL_X: f32 = 0.52;
const WHEEL_SIDES: usize = 10;

fn rgba(rgb: Rgb) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], MATERIAL_PLAIN]
}

/// How the rider and the bike look: every colour the meshes are built from.
///
/// TODO(avatar): let riders make their own — build, outfit, bike. Only the
/// default exists today, from the palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Avatar {
    pub helmet: Rgb,
    pub hair: Rgb,
    pub skin: Rgb,
    pub jersey: Rgb,
    /// Shorts, shoes, gloves' grip, the helmet's vents and straps.
    pub kit: Rgb,
    pub frame: Rgb,
    pub tyre: Rgb,
}

impl Default for Avatar {
    fn default() -> Self {
        let p = &palette::BASE;
        Self {
            helmet: p.helmet,
            hair: p.hair,
            skin: p.skin,
            jersey: p.jersey,
            kit: p.kit,
            frame: p.frame,
            tyre: p.tyre,
        }
    }
}

impl Avatar {
    /// The ghost rider: the rider's own shape, pale all over, its kit and
    /// tyres a soft blue so the shape still reads.
    pub fn ghost() -> Self {
        let p = &palette::BASE;
        Self {
            helmet: p.ghost,
            hair: p.ghost,
            skin: p.ghost,
            jersey: p.ghost,
            kit: p.ghost_trim,
            frame: p.ghost,
            tyre: p.ghost_trim,
        }
    }
}

/// A wheel: a flat many-sided disc standing upright, centred at `x`.
fn wheel(x: f32, tyre: Rgb) -> Mesh {
    let tyre = rgba(tyre);
    let mut m = Mesh::default();
    for half in [-0.03f32, 0.03] {
        let centre = m.vertices.len() as u32;
        m.vertices.push(Vertex {
            pos: [x, WHEEL_R, half],
            colour: tyre,
        });
        for s in 0..WHEEL_SIDES {
            // From the bottom round, so a corner touches the road.
            let a =
                s as f32 / WHEEL_SIDES as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            m.vertices.push(Vertex {
                pos: [x + WHEEL_R * a.cos(), WHEEL_R + WHEEL_R * a.sin(), half],
                colour: tyre,
            });
        }
        for s in 0..WHEEL_SIDES as u32 {
            let next = (s + 1) % WHEEL_SIDES as u32;
            m.indices
                .extend_from_slice(&[centre, centre + 1 + s, centre + 1 + next]);
        }
    }
    // The tread joining the two sides.
    for s in 0..WHEEL_SIDES as u32 {
        let next = (s + 1) % WHEEL_SIDES as u32;
        let (a, b) = (1 + s, 1 + next);
        let off = WHEEL_SIDES as u32 + 1;
        m.indices
            .extend_from_slice(&[a, b, b + off, a, b + off, a + off]);
    }
    m
}

/// Everything but the legs: bike and rider.
pub fn body(a: &Avatar) -> Mesh {
    let (frame, jersey, helmet, skin, kit, hair) = (
        rgba(a.frame),
        rgba(a.jersey),
        rgba(a.helmet),
        rgba(a.skin),
        rgba(a.kit),
        rgba(a.hair),
    );
    let mut m = Mesh::default();
    m.extend(wheel(-WHEEL_X, a.tyre));
    m.extend(wheel(WHEEL_X, a.tyre));
    // Frame: chainstays, seat tube, top tube, down tube, fork, bars.
    m.extend(cuboid([-WHEEL_X, 0.31, -0.03], [-0.12, 0.37, 0.03], frame));
    m.extend(cuboid([-0.18, 0.34, -0.03], [-0.11, 0.86, 0.03], frame));
    m.extend(cuboid([-0.18, 0.76, -0.03], [0.44, 0.81, 0.03], frame));
    m.extend(cuboid([-0.12, 0.36, -0.03], [0.44, 0.43, 0.03], frame));
    m.extend(cuboid([0.42, WHEEL_R, -0.03], [0.48, 0.86, 0.03], frame));
    m.extend(cuboid([0.42, 0.86, -0.22], [0.5, 0.9, 0.22], kit));
    // Rider: saddle, hips, a back bent over the bars, arms, head.
    m.extend(cuboid([-0.26, 0.86, -0.08], [-0.02, 0.92, 0.08], kit));
    m.extend(cuboid([-0.26, 0.9, -0.17], [0.0, 1.06, 0.17], kit));
    m.extend(cuboid([-0.2, 1.02, -0.19], [0.32, 1.3, 0.19], jersey));
    for z in [-0.2f32, 0.2] {
        m.extend(cuboid([0.28, 0.9, z - 0.05], [0.4, 1.24, z + 0.05], skin));
    }
    m.extend(cuboid([0.3, 1.24, -0.11], [0.52, 1.44, 0.11], skin));
    // Hair over the back and sides of the head, showing below the helmet.
    m.extend(cuboid(HAIR_FROM, HAIR_TO, hair));
    // The helmet: a shell sitting on top of the hair, drawn out to a tail at
    // the back, dark vents along its crown and a strap down either cheek.
    m.extend(cuboid(HELMET_FROM, HELMET_TO, helmet));
    // A narrower crown, so it rounds off as a helmet does, not a box.
    m.extend(cuboid(
        [0.3, HELMET_TO[1], -0.1],
        [0.52, CROWN_TOP, 0.1],
        helmet,
    ));
    m.extend(cuboid([0.22, 1.42, -0.1], [0.28, 1.49, 0.1], helmet));
    for z in [-0.07f32, 0.03] {
        m.extend(cuboid(
            [0.33, CROWN_TOP, z],
            [0.5, CROWN_TOP + 0.01, z + 0.04],
            kit,
        ));
    }
    for z in [HAIR_FROM[2] - 0.006, HAIR_TO[2]] {
        m.extend(cuboid(
            [0.44, 1.27, z],
            [0.46, HELMET_FROM[1], z + 0.006],
            kit,
        ));
    }
    m
}

/// The hair's box: the back and sides of the head, below the helmet's rim.
const HAIR_FROM: [f32; 3] = [0.285, 1.27, -0.118];
const HAIR_TO: [f32; 3] = [0.44, 1.43, 0.118];
/// The helmet's shell.
const HELMET_FROM: [f32; 3] = [0.27, 1.39, -0.14];
const HELMET_TO: [f32; 3] = [0.55, 1.5, 0.14];
const CROWN_TOP: f32 = 1.56;

/// One leg, hung from the hip at the origin: hanging down to the pedal.
pub fn leg(a: &Avatar) -> Mesh {
    let mut m = Mesh::default();
    m.extend(cuboid(
        [-0.07, -0.42, -0.06],
        [0.07, 0.0, 0.06],
        rgba(a.kit),
    ));
    m.extend(cuboid(
        [-0.05, -0.8, -0.05],
        [0.05, -0.42, 0.05],
        rgba(a.skin),
    ));
    m.extend(cuboid(
        [-0.05, -0.86, -0.06],
        [0.15, -0.8, 0.06],
        rgba(a.kit),
    ));
    m
}

/// How far round the cranks are after `distance_m` of riding, in radians:
/// the legs turn with the road, so they stop when the rider stops.
pub fn crank(distance_m: f32) -> f32 {
    (distance_m / METRES_PER_TURN).rem_euclid(1.0) * std::f32::consts::TAU
}

/// Highest cadence the legs are turned by, in rpm: a sensor claiming more
/// is not believed (CLAUDE.md §5.1).
const MAX_CADENCE_RPM: f32 = 250.0;
/// How quickly the legs take up a new cadence, in seconds: a sensor reports
/// about once a second, and a step from 70 to 95 rpm in one frame is a
/// visible lurch of the feet.
const CADENCE_GLIDE_S: f32 = 0.5;

/// The cranks as drawn, turning at the rider's measured cadence — or, with
/// no cadence sensor, with the road, as [`crank`] turns them.
///
/// The angle is carried on frame by frame rather than worked out afresh,
/// so a sensor dropping out or coming back changes how fast the legs turn,
/// never where they are.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cranks {
    /// How far round, in radians, in `0..TAU`.
    angle: f32,
    /// How fast they turn, in radians a second.
    rate: f32,
    /// The distance and time of the last frame, in metres and seconds —
    /// `None` before the first.
    last: Option<(f32, f64)>,
}

impl Cranks {
    /// Turn the cranks on to `now_s`, the rider `distance_m` along, pedalling
    /// at `cadence_rpm` — `None` with no cadence to go by; and how far round
    /// they are.
    pub fn advance(&mut self, now_s: f64, distance_m: f32, cadence_rpm: Option<f32>) -> f32 {
        let tau = std::f32::consts::TAU;
        let distance = if distance_m.is_finite() {
            distance_m
        } else {
            0.0
        };
        let Some((last_m, last_s)) = self.last else {
            self.last = Some((distance, now_s));
            self.angle = match cadence_rpm {
                Some(_) => 0.0,
                None => crank(distance),
            };
            self.rate = cadence_rpm.map_or(0.0, rate_of);
            return self.angle;
        };
        let dt = if now_s.is_finite() {
            (now_s - last_s).max(0.0) as f32
        } else {
            0.0
        };
        self.last = Some((distance, if now_s.is_finite() { now_s } else { last_s }));
        if dt <= 0.0 {
            return self.angle;
        }
        let target = match cadence_rpm {
            Some(c) => rate_of(c),
            None => {
                // With the road; a skip along the route is no pedalling.
                let rolled = distance - last_m;
                if (0.0..=100.0).contains(&rolled) {
                    rolled / METRES_PER_TURN * tau / dt
                } else {
                    self.rate
                }
            }
        };
        self.rate += (target - self.rate) * (1.0 - (-dt / CADENCE_GLIDE_S).exp());
        self.angle = (self.angle + self.rate * dt).rem_euclid(tau);
        if self.angle >= tau {
            self.angle = 0.0;
        }
        self.angle
    }
}

/// `cadence_rpm` as radians a second, within what a rider can turn.
fn rate_of(cadence_rpm: f32) -> f32 {
    let rpm = if cadence_rpm.is_finite() {
        cadence_rpm.clamp(0.0, MAX_CADENCE_RPM)
    } else {
        0.0
    };
    rpm / 60.0 * std::f32::consts::TAU
}

/// A leg's swing at the hip (radians, forward positive) and its lift
/// (metres) at crank angle `crank`; `side` is −1 for the left leg, 1 for the
/// right, half a turn apart.
pub fn leg_pose(crank: f32, side: f32) -> (f32, f32) {
    let a = crank
        + if side < 0.0 {
            std::f32::consts::PI
        } else {
            0.0
        };
    (SWING_RAD * a.sin(), LIFT_M * a.cos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_stand_the_bike_on_the_ground() {
        let m = body(&Avatar::default());
        let lo = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MAX, f32::min);
        assert!(lo.abs() < 1e-5, "{lo}");
        let hi = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MIN, f32::max);
        assert!((1.4..1.8).contains(&hi), "a rider {hi} m tall on the bike");
        let n = m.vertices.len() as u32;
        assert!(m.indices.iter().all(|&i| i < n));
    }

    #[test]
    fn should_fit_the_rider_inside_the_lane() {
        // The rider rides the middle of a 2.6 m lane: elbows and bars stay
        // well within half of it.
        for v in body(&Avatar::default())
            .vertices
            .iter()
            .chain(leg(&Avatar::default()).vertices.iter())
        {
            assert!(v.pos[2].abs() + LEG_Z < 0.6, "{:?}", v.pos);
        }
    }

    #[test]
    fn should_hang_a_leg_from_the_hip_to_the_pedal() {
        let m = leg(&Avatar::default());
        let top = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MIN, f32::max);
        let foot = m.vertices.iter().map(|v| v.pos[1]).fold(f32::MAX, f32::min);
        assert_eq!(top, 0.0);
        // Hung from the hip, the foot comes down near the pedals, above the
        // ground.
        assert!(
            HIP[1] + foot > 0.0 && HIP[1] + foot < 0.2,
            "{}",
            HIP[1] + foot
        );
    }

    #[test]
    fn should_wear_a_helmet_over_hair_that_shows_below_it_at_the_back() {
        let a = Avatar::default();
        assert_ne!(
            a.helmet, a.hair,
            "a helmet the colour of hair reads as hair"
        );
        let m = body(&a);
        let of = |rgb: Rgb| -> Vec<[f32; 3]> {
            m.vertices
                .iter()
                .filter(|v| v.colour[..3] == rgb)
                .map(|v| v.pos)
                .collect()
        };
        let (helmet, hair, skin) = (of(a.helmet), of(a.hair), of(a.skin));
        let top = |ps: &[[f32; 3]]| ps.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
        let low = |ps: &[[f32; 3]]| ps.iter().map(|p| p[1]).fold(f32::MAX, f32::min);
        let back = |ps: &[[f32; 3]]| ps.iter().map(|p| p[0]).fold(f32::MAX, f32::min);
        // The helmet is the top of the rider, wider than the head.
        assert!(top(&helmet) > top(&hair) && top(&helmet) > top(&skin));
        let wide = |ps: &[[f32; 3]]| ps.iter().map(|p| p[2].abs()).fold(0.0, f32::max);
        assert!(wide(&helmet) > wide(&hair));
        // Seen from behind, hair shows between the helmet's rim and the neck.
        assert!(low(&hair) < low(&helmet) - 0.08, "no hair below the helmet");
        let head_back = skin
            .iter()
            .filter(|p| p[1] > 1.24)
            .map(|p| p[0])
            .fold(f32::MAX, f32::min);
        assert!(
            back(&hair) < head_back,
            "the hair covers the back of the head"
        );
    }

    #[test]
    fn should_build_the_rider_in_the_avatars_colours() {
        let a = Avatar {
            jersey: [0.1, 0.9, 0.2],
            hair: [0.9, 0.1, 0.1],
            ..Avatar::default()
        };
        let m = body(&a);
        assert!(m.vertices.iter().any(|v| v.colour[..3] == [0.1, 0.9, 0.2]));
        assert!(m.vertices.iter().any(|v| v.colour[..3] == [0.9, 0.1, 0.1]));
        assert!(!m
            .vertices
            .iter()
            .any(|v| v.colour[..3] == palette::BASE.jersey));
    }

    #[test]
    fn should_turn_the_cranks_with_the_distance_and_stop_with_the_rider() {
        assert_eq!(crank(0.0), 0.0);
        assert!((crank(METRES_PER_TURN / 4.0) - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        assert!(
            crank(METRES_PER_TURN).abs() < 1e-4
                || (crank(METRES_PER_TURN) - std::f32::consts::TAU).abs() < 1e-4
        );
        assert_eq!(
            crank(123.4),
            crank(123.4),
            "standing still, the legs hold still"
        );
        assert!((0.0..std::f32::consts::TAU).contains(&crank(-3.0)));
    }

    #[test]
    fn should_move_the_legs_half_a_turn_apart() {
        for k in 0..16 {
            let c = k as f32 * 0.4;
            let (l, ll) = leg_pose(c, -1.0);
            let (r, rl) = leg_pose(c, 1.0);
            assert!((l + r).abs() < 1e-5 && (ll + rl).abs() < 1e-5, "at {c}");
            assert!(l.abs() <= SWING_RAD + 1e-6);
        }
    }

    /// Frames at 60 a second from `cranks`, starting at `from_s`: the angle
    /// at each, the rider at `distance(k)` pedalling at `cadence(k)`.
    fn frames(
        cranks: &mut Cranks,
        n: usize,
        distance: &dyn Fn(usize) -> f32,
        cadence: &dyn Fn(usize) -> Option<f32>,
    ) -> Vec<f32> {
        (0..n)
            .map(|k| cranks.advance(k as f64 / 60.0, distance(k), cadence(k)))
            .collect()
    }

    #[test]
    fn should_turn_the_legs_at_the_measured_cadence() {
        // 90 rpm is a turn and a half a second: after one second exactly,
        // half a turn on from where they started.
        let mut c = Cranks::default();
        let a = frames(&mut c, 61, &|k| k as f32 * 0.15, &|_| Some(90.0));
        assert_eq!(a[0], 0.0);
        assert!((a[60] - std::f32::consts::PI).abs() < 1e-3, "{}", a[60]);
        // Whatever the speed: the same cadence at a crawl turns them as fast.
        let mut slow = Cranks::default();
        let b = frames(&mut slow, 61, &|k| k as f32 * 0.01, &|_| Some(90.0));
        assert!((b[60] - std::f32::consts::PI).abs() < 1e-3, "{}", b[60]);
    }

    #[test]
    fn should_hold_the_legs_still_when_coasting() {
        // Rolling at 10 m/s with the cadence sensor saying 0: freewheeling.
        let mut c = Cranks::default();
        let a = frames(&mut c, 120, &|k| k as f32 / 6.0, &|_| Some(0.0));
        assert!(a.iter().all(|&x| x == 0.0), "{a:?}");
    }

    #[test]
    fn should_turn_the_legs_with_the_road_without_a_cadence_sensor() {
        // 9 m/s with no cadence sensor: a turn every 6.5 m, as the gear
        // says — 83.1 rpm — and still once the rider stops.
        let mut c = Cranks::default();
        let a = frames(&mut c, 600, &|k| 500.0 + k.min(300) as f32 * 0.15, &|_| {
            None
        });
        let tau = std::f32::consts::TAU;
        let rpm = (a[241] - a[240]).rem_euclid(tau) * 60.0 / tau * 60.0;
        let want = 9.0 / METRES_PER_TURN * 60.0;
        assert!((rpm - want).abs() < 0.1, "{rpm} rpm, not {want}");
        assert!((a[599] - a[598]).abs() < 1e-4, "still turning once stopped");
    }

    #[test]
    fn should_change_cadence_without_a_lurch() {
        // 70 rpm, then the sensor says 100 and later drops out; at no frame
        // may the legs jump, only speed up or slow down a little.
        let mut c = Cranks::default();
        let a = frames(&mut c, 360, &|k| k as f32 * 0.15, &|k| match k {
            0..=119 => Some(70.0),
            120..=239 => Some(100.0),
            _ => None,
        });
        let tau = std::f32::consts::TAU;
        let steps: Vec<f32> = a
            .windows(2)
            .map(|w| (w[1] - w[0]).rem_euclid(tau))
            .collect();
        for (k, w) in steps.windows(2).enumerate() {
            // One frame's turn changes by at most what 6 rpm a frame-step is.
            let change = (w[1] - w[0]).abs();
            assert!(change < 0.02, "frame {k}: the turn changed by {change} rad");
        }
        // Settled on 100 rpm by the end of the second second.
        let at_100 = steps[235] * 60.0 / tau * 60.0;
        assert!((at_100 - 100.0).abs() < 1.0, "turning at {at_100} rpm");
    }

    #[test]
    fn should_survive_nonsense_from_a_sensor() {
        let mut c = Cranks::default();
        for (t, d, cad) in [
            (0.0, 0.0, Some(f32::NAN)),
            (0.1, f32::NAN, Some(90.0)),
            (f64::NAN, 1.0, Some(f32::INFINITY)),
            (0.2, 2.0, Some(-40.0)),
            (0.1, 2.0, Some(90.0)),
            (0.3, 1e9, None),
        ] {
            let a = c.advance(t, d, cad);
            assert!((0.0..std::f32::consts::TAU).contains(&a), "{a}");
        }
        // A rogue 65535 rpm turns the legs no faster than 250.
        let mut rogue = Cranks::default();
        let a = frames(&mut rogue, 61, &|_| 0.0, &|_| Some(65_535.0));
        let mut sane = Cranks::default();
        let b = frames(&mut sane, 61, &|_| 0.0, &|_| Some(250.0));
        assert_eq!(a, b);
    }
}
