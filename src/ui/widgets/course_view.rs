//! The road from behind the rider: a 3D view of the route, drawn with OpenGL.
//!
//! Everything it draws comes from [`crate::data::course3d`]; this module only
//! uploads that geometry, moves the camera and the rider, and lets the
//! animals move. The ride loop reports distance once a second, and a camera
//! that jumped once a second would be unwatchable, so between reports the
//! view carries the rider on at their last speed — for at most a second, so
//! a paused ride stops instead of driving on.
//!
//! The world is made up from the route alone (`data/world.rs`): nothing is
//! downloaded, and a route rides the same with or without a network. It is
//! built off the main thread when the route is set.
//!
//! GL is reached through `eglGetProcAddress`. GTK renders through EGL on
//! Wayland and on X11 alike, and the GNOME runtime ships libEGL through
//! libglvnd, whose function pointers work for whichever context is current.

use adw::prelude::*;
use glow::HasContext;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CString};
use std::rc::Rc;
use std::sync::OnceLock;

use crate::data::biome;
use crate::data::course3d::{self, CameraPose, CourseScene, Mat4, Mesh, RiderPose, Sky};
use crate::data::encounters;
use crate::data::ghost::Ghost;
use crate::data::palette;
use crate::data::props::{self, Prop, Shape};
use crate::data::rider;
use crate::data::route::Route;
use crate::ui::widgets::ride_hud::{self, FinishedCard, HudReadings, RideHud};

/// Vertical field of view, in degrees. Wide enough that a bend does not
/// leave the frame.
const FOV_DEG: f32 = 60.0;
const NEAR_M: f32 = 0.5;
/// Past the mountains on the horizon.
const FAR_M: f32 = 9_000.0;

/// Where the sun is: high in the south-west, so slopes facing it are lit
/// and slopes facing away fall into shade. Not normalised here; the shader does.
const SUN_DIR: [f32; 3] = [-0.45, 0.85, 0.40];

/// The clouds drift round the sky this fast, in radians a second.
const CLOUD_DRIFT: f32 = 0.0025;

/// Props are drawn only in chunks this near the eye: the haze has the rest.
const PROP_DRAW_M: f32 = 1_300.0;

const BUILDING: &str = "Building the 3D view…";

/// Longest the view will carry the rider on without a fresh distance, in
/// seconds. A tick is due every second; past that, the ride has paused.
const MAX_EXTRAPOLATE_S: f32 = 1.0;

/// How quickly the drawn rider closes on where the ride says they are, in
/// seconds: the time to make up two thirds of the gap. A report is due every
/// second, so the gap is made up at about its own size a second — a metre
/// short adds a metre a second for a moment, not a lurch.
const GLIDE_S: f32 = 1.0;
/// How quickly the speed the rider leans by follows the ride's, in seconds.
/// The ride's speed changes in a step at each report, and in a hairpin the
/// lean is so sensitive to speed that each step tipped the rider over by
/// several degrees at once.
const LEAN_GLIDE_S: f32 = 0.6;
/// Over the finish line the rider coasts to a stop, slowing by this time
/// constant, in seconds: rolling on about as many metres as they rode in
/// it, past the arch.
const COAST_S: f32 = 2.5;
/// How much of the ghost rider shows: enough to read as a rider, little
/// enough to read as not there.
const GHOST_ALPHA: f32 = 0.65;
/// A gap wider than this, in metres, is a jump — a skip along the route —
/// and is taken in one frame, not glided over.
const SNAP_M: f32 = 30.0;

/// A camera that moved less than this since the last frame is not redrawn —
/// unless something in the world is moving.
const REDRAW_EPSILON_M: f32 = 0.001;

/// The world's vertices, placed by `u_model` (the identity for the land; the
/// rider's pose for the rider; the eye for the horizon).
const VERTEX_SHADER: &str = r#"
in vec3 a_pos;
in vec4 a_colour;
uniform mat4 u_mvp;
uniform mat4 u_model;
uniform vec3 u_eye;
out vec3 v_world;
out vec3 v_rel;
// Flat: each triangle takes one vertex's colour whole, so faces read as
// facets, not gradients.
flat out vec4 v_colour;
flat out vec3 v_anchor;
void main() {
    vec3 world = (u_model * vec4(a_pos, 1.0)).xyz;
    v_world = world;
    // Relative to the eye, the numbers are small. Interpolated across a pixel
    // at thousands of metres from the scene's origin, the absolute position
    // loses the precision the lighting takes its slope from, and walls came
    // out speckled.
    v_rel = world - u_eye;
    v_colour = a_colour;
    v_anchor = world;
    gl_Position = u_mvp * vec4(world, 1.0);
}
"#;

/// A prop: one of the few shapes, scaled, turned and set down where it
/// stands, in its own colour where the shape asks for it. Animals move here:
/// a grazing sheep turns this way and that and dips its head; a bird circles
/// its spot, beating its wings. Feeds the same fragment shader as the world.
const PROP_VERTEX_SHADER: &str = r#"
in vec3 a_pos;
in vec4 a_colour;
in vec3 i_pos;
in vec2 i_size;
in vec3 i_colour;
in vec2 i_motion;
uniform mat4 u_mvp;
uniform vec3 u_eye;
uniform float u_time;
// Small props shrink away between these distances from the eye, in metres,
// rather than blinking out where they stop being drawn.
uniform vec2 u_reach;
uniform float u_orbit;
uniform float u_head;
uniform vec2 u_hub;
uniform float u_legs;
out vec3 v_world;
out vec3 v_rel;
flat out vec4 v_colour;
flat out vec3 v_anchor;
void main() {
    vec3 base = i_pos;
    vec3 p = a_pos;
    float yaw = i_size.y;
    float phase = i_motion.y;
    float motion = floor(i_motion.x + 0.5);
    if (motion == 1.0) {
        yaw += 0.35 * sin(u_time * 0.25 + phase);
        if (p.x > u_head) {
            float dip = 0.5 + 0.5 * sin(u_time * 0.8 + phase * 3.0);
            p.y -= 0.3 * dip;
            p.x -= 0.06 * dip;
        }
    } else if (motion == 4.0) {
        // Sails: everything in front of the hub turns about it.
        if (p.x > u_hub.x) {
            float a = u_time * 0.9 + phase;
            float c0 = cos(a);
            float s0 = sin(a);
            vec2 q = vec2(p.z, p.y - u_hub.y);
            p.z = q.x * c0 - q.y * s0;
            p.y = u_hub.y + q.x * s0 + q.y * c0;
        }
    } else if (motion == 3.0) {
        // Afloat: rising and falling, turning a little on the swell.
        base.y += 0.12 * sin(u_time * 1.3 + phase);
        yaw += 0.08 * sin(u_time * 0.5 + phase * 2.0);
    } else if (motion == 2.0) {
        float a = u_time * 0.35 + phase;
        base += vec3(cos(a) * u_orbit, 1.5 * sin(u_time * 0.6 + phase), sin(a) * u_orbit);
        yaw = a + 1.5707963;
        p.y += sin(u_time * 9.0 + phase * 5.0) * 0.6 * abs(p.z);
    } else if (motion == 5.0) {
        // Running: legs below the body swing in pairs, fore against hind
        // and left against right; the body bounces with the stride.
        if (p.y < u_legs) {
            float pair = (p.x > 0.0 ? 0.0 : 3.1415927) + (p.z > 0.0 ? 1.5707963 : 0.0);
            p.x += 0.16 * sin(u_time * 13.0 + phase + pair) * (u_legs - p.y) / u_legs;
        }
        base.y += 0.06 * i_size.x * abs(sin(u_time * 13.0 + phase));
    } else if (motion == 6.0) {
        // A snake: the body waves side to side, travelling down it.
        p.z += 0.09 * sin(p.x * 11.0 - u_time * 9.0 + phase);
    } else if (motion == 7.0) {
        // Wings beating, the bird flown along by the encounter.
        p.y += sin(u_time * 7.0 + phase * 5.0) * 0.5 * abs(p.z);
    } else if (motion == 8.0) {
        // Rolling forward about its centre, half a unit up.
        float a = -(u_time * 5.0 + phase);
        vec2 q = vec2(p.x, p.y - 0.5);
        p.x = q.x * cos(a) - q.y * sin(a);
        p.y = 0.5 + q.x * sin(a) + q.y * cos(a);
    }
    float c = cos(yaw);
    float s = sin(yaw);
    p *= i_size.x * (1.0 - smoothstep(u_reach.x, u_reach.y, length(base - u_eye)));
    vec3 world = base + vec3(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
    v_colour = a_colour.r < 0.0 ? vec4(i_colour, a_colour.a) : a_colour;
    v_world = world;
    v_rel = world - u_eye;
    v_anchor = world;
    gl_Position = u_mvp * vec4(world, 1.0);
}
"#;

/// The painted light. No lines: shapes stand apart by light alone — warm in
/// the sun, turning cool and blue as a face turns away, the sky's blue on
/// faces that look up, a warm glow along the edges of walls, trunks and
/// crowns, and the shadows of clouds drifting over everything. Then the
/// middle distance goes blue in layers, and the haze takes the rest. The
/// horizon's mountains take a fixed haze of their own, thicker at their
/// feet; clouds take none.
const FRAGMENT_SHADER: &str = r#"
in vec3 v_world;
in vec3 v_rel;
flat in vec4 v_colour;
flat in vec3 v_anchor;
uniform vec3 u_horizon;
uniform vec3 u_zenith;
uniform vec3 u_sun;
uniform vec3 u_foam;
uniform float u_fog_start;
uniform float u_fog_density;
uniform float u_time;
uniform vec3 u_sunlit;
uniform vec3 u_shade;
uniform vec3 u_rim;
uniform vec3 u_haze;
uniform vec3 u_patch_cool;
uniform vec3 u_patch_warm;
uniform vec3 u_crack;
// 1 but for the ghost rider, drawn see-through.
uniform float u_alpha;
out vec4 frag;

float hash(vec3 p) {
    return fract(sin(dot(p, vec3(127.1, 311.7, 74.7))) * 43758.5453);
}

// Smooth noise over the ground plane, for the clouds' shadows.
float noise2(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    float a = hash(vec3(i, 0.0));
    float b = hash(vec3(i + vec2(1.0, 0.0), 0.0));
    float c = hash(vec3(i + vec2(0.0, 1.0), 0.0));
    float d = hash(vec3(i + vec2(1.0, 1.0), 0.0));
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

// A hash without sine, steady at the scene's thousands of metres.
float hash13(vec3 p) {
    p = fract(p * 0.1031);
    p += dot(p, p.zyx + 31.32);
    return fract((p.x + p.y) * p.z);
}

// Stone: the world cut into rounded blocks. Returns how far this point is
// from the nearest crack between two blocks, and a tone for its block.
vec2 stone(vec3 p) {
    vec3 i = floor(p);
    vec3 f = fract(p);
    float d1 = 8.0;
    float d2 = 8.0;
    float tone = 0.0;
    for (int x = -1; x <= 1; x++) {
        for (int y = -1; y <= 1; y++) {
            for (int z = -1; z <= 1; z++) {
                vec3 g = vec3(float(x), float(y), float(z));
                vec3 o = vec3(hash13(i + g), hash13(i + g + 17.0), hash13(i + g + 31.0));
                float d = length(g + o - f);
                if (d < d1) {
                    d2 = d1;
                    d1 = d;
                    tone = hash13(i + g + 5.0);
                } else if (d < d2) {
                    d2 = d;
                }
            }
        }
    }
    return vec2(d2 - d1, tone);
}

void main() {
    vec3 c = v_colour.rgb;
    float material = floor(v_colour.a + 0.5);
    // The face normal comes from how the position changes across the pixel,
    // so no normals are stored and every face is flat.
    vec3 n = normalize(cross(dFdx(v_rel), dFdy(v_rel)));
    // Face the viewer. Flipping to face *up* instead decides a wall's side by
    // the sign of a vertical component that is noise, and the wall comes out
    // speckled light and dark pixel by pixel.
    if (dot(n, v_rel) > 0.0) n = -n;
    float away = length(v_rel);
    if (material == 1.0) {
        // A hand-made look: each facet of the land a few per cent lighter or
        // darker, seeded by where it is.
        c *= 0.96 + 0.08 * hash(floor(v_anchor));
        if (n.y > 0.5) {
            // Painted patches across the grass, cooler and warmer, and up
            // close long soft brush strokes. The strokes go before they are
            // small enough to shimmer.
            vec2 g = v_world.xz;
            float blotch = 0.6 * noise2(g / 22.0) + 0.4 * noise2(g / 8.0 + 3.1);
            c *= mix(u_patch_cool, u_patch_warm, smoothstep(0.3, 0.7, blotch));
            float stroke = noise2(vec2(g.x * 1.4 + g.y * 0.5, g.y * 0.35 - g.x * 0.2));
            c *= 1.0 + 0.08 * (stroke - 0.5) * (1.0 - smoothstep(25.0, 60.0, away));
        }
    } else if (material == 3.0) {
        // Rock: blocks of their own tone, split by darker cracks. Both go
        // with distance, before a crack is thinner than a pixel.
        vec2 st = stone(v_world / 1.7);
        float near = 1.0 - smoothstep(60.0, 160.0, away);
        float crack = 1.0 - smoothstep(0.05, 0.05 + 1.5 * fwidth(st.x), st.x);
        c *= mix(1.0, 0.86 + 0.24 * st.y, near);
        c *= mix(vec3(1.0), u_crack, crack * near);
    } else if (material == 4.0) {
        // Water lies flat, with lighter stripes drifting along it.
        n = vec3(0.0, 1.0, 0.0);
        float w = sin(v_world.x * 0.21 + v_world.z * 0.13 + u_time * 1.6)
            * sin(v_world.z * 0.17 - v_world.x * 0.07 - u_time * 1.1);
        c = mix(c, u_foam, 0.35 * step(0.55, w));
    }
    if (material == 6.0) {
        // Clouds: white, a little shade underneath, no haze.
        frag = vec4(c * (0.9 + 0.1 * clamp(n.y, 0.0, 1.0)) - 0.06 * clamp(-n.y, 0.0, 1.0), 1.0);
        return;
    }
    float sunward = smoothstep(0.0, 0.45, dot(n, normalize(u_sun)));
    // Big, soft cloud shadows: smaller ones read as camouflage on a hill.
    vec2 q = v_world.xz / 420.0 + vec2(u_time * 0.004, u_time * 0.0015);
    float cloud = smoothstep(0.5, 0.8, 0.7 * noise2(q) + 0.3 * noise2(q * 2.1 + 7.0));
    sunward *= 1.0 - 0.6 * cloud;
    vec3 lit = c * mix(u_shade, u_sunlit, sunward);
    lit += c * u_zenith * 0.10 * max(n.y, 0.0);
    // The glow is for walls, trunks and crowns, not the ground: at a low
    // angle every field would glow.
    float edge_glow = pow(1.0 - clamp(dot(n, normalize(-v_rel)), 0.0, 1.0), 3.0);
    lit += u_rim * edge_glow * 0.25 * sunward * (1.0 - abs(n.y));
    if (material == 5.0) {
        // Far mountains: pale and blue with distance, palest at their feet.
        float up = clamp((v_rel.y + 120.0) / 1100.0, 0.0, 1.0);
        frag = vec4(mix(lit, mix(u_horizon, u_haze, 0.6), mix(0.72, 0.32, up)), 1.0);
        return;
    }
    lit = mix(lit, u_haze, 0.6 * (1.0 - exp(-away / 450.0)));
    float k = max(away - u_fog_start, 0.0) * u_fog_density;
    frag = vec4(mix(lit, u_horizon, 1.0 - exp(-k * k)), u_alpha);
}
"#;

/// One triangle over the whole view, for the sky, the finishing pass and
/// the HUD.
const FULLSCREEN_VERTEX_SHADER: &str = r#"
out vec2 v_uv;
void main() {
    vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
    v_uv = p * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

/// The fullscreen ride HUD, painted by Cairo, over everything. Cairo's pixels
/// lie in memory as B, G, R, A, alpha already multiplied in, and run top to
/// bottom.
const HUD_FRAGMENT_SHADER: &str = r#"
precision highp sampler2D;
in vec2 v_uv;
uniform sampler2D u_hud;
out vec4 frag;
void main() {
    frag = texture(u_hud, vec2(v_uv.x, 1.0 - v_uv.y)).bgra;
}
"#;

/// The sky: a bright band at the horizon, deepening to blue overhead. Drawn
/// first, without depth, under everything.
const SKY_FRAGMENT_SHADER: &str = r#"
in vec2 v_uv;
uniform vec3 u_horizon;
uniform vec3 u_zenith;
uniform vec3 u_glow;
out vec4 frag;
void main() {
    vec3 low = mix(u_horizon, u_glow, 0.35);
    float rise = pow(smoothstep(0.45, 1.0, v_uv.y), 0.7);
    frag = vec4(mix(low, u_zenith, rise), 1.0);
}
"#;

/// The finishing pass, from the scene's framebuffer into GTK's: colours a
/// little richer, and the corners darker and cooler, as a painter frames a
/// scene.
const FINISH_FRAGMENT_SHADER: &str = r#"
precision highp sampler2D;
in vec2 v_uv;
uniform sampler2D u_colour;
uniform vec3 u_corner;
out vec4 frag;
void main() {
    vec3 c = texture(u_colour, v_uv).rgb;
    float grey = dot(c, vec3(0.299, 0.587, 0.114));
    c = mix(vec3(grey), c, 1.08);
    vec2 off = v_uv - vec2(0.5, 0.55);
    float corner = smoothstep(0.35, 0.85, length(off * vec2(1.0, 1.25)));
    frag = vec4(c * mix(vec3(1.0), u_corner, corner), 1.0);
}
"#;

/// The distance last reported by the ride, and when.
#[derive(Debug, Clone, Copy, Default)]
struct Progress {
    distance_m: f32,
    speed_ms: f32,
    /// `glib::monotonic_time()` of the report, in µs — the frame clock's own
    /// time base. `None` before the ride has reported at all.
    at_us: Option<i64>,
    /// Over the line: no more reports come, and the rider rolls to a stop.
    coast: bool,
}

impl Progress {
    /// Where the rider is at `now_us`: the last report carried on at its speed
    /// for at most [`MAX_EXTRAPOLATE_S`].
    fn distance_at(&self, now_us: i64) -> f32 {
        let Some(at) = self.at_us else {
            return self.distance_m;
        };
        if self.coast {
            let t = ((now_us - at) as f32 / 1e6).max(0.0);
            let v = if self.speed_ms.is_finite() {
                self.speed_ms.max(0.0)
            } else {
                0.0
            };
            return self.distance_m + v * COAST_S * (1.0 - (-t / COAST_S).exp());
        }
        let dt = ((now_us - at) as f32 / 1e6).clamp(0.0, MAX_EXTRAPOLATE_S);
        self.distance_m + self.speed_ms.max(0.0) * dt
    }

    /// How fast the rider is going at `now_us`, as far as the view carries
    /// them: the last report's speed, and none once the view has stopped
    /// carrying them on — or before any report.
    fn speed_at(&self, now_us: i64) -> f32 {
        let Some(at) = self.at_us else {
            return 0.0;
        };
        let dt = (now_us - at) as f32 / 1e6;
        if self.coast && self.speed_ms.is_finite() {
            return self.speed_ms.max(0.0) * (-dt.max(0.0) / COAST_S).exp();
        }
        if dt < MAX_EXTRAPOLATE_S && self.speed_ms.is_finite() {
            self.speed_ms.max(0.0)
        } else {
            0.0
        }
    }
}

/// Where the rider is drawn, gliding onto where the ride says they are.
///
/// Between reports the view carries the rider on at the last speed, and the
/// next report is rarely exactly there: braking into a bend or easing up a
/// climb, the ride is a metre short of where the view has drawn them. Taken
/// as it came, every report was a jolt back up the road — in every bend.
/// So the drawn rider goes on at the ride's speed and makes up the gap
/// smoothly, and never backwards.
#[derive(Debug, Clone, Copy, Default)]
struct Glide {
    /// The distance drawn last frame, and when — `None` before the first.
    shown: Option<(f32, i64)>,
    /// The speed the rider is drawn leaning by, in m/s.
    lean_speed: f32,
}

impl Glide {
    /// Where to draw the rider at `now_us`, with the ride putting them at
    /// `target_m`, going `speed_ms`.
    fn follow(&mut self, target_m: f32, speed_ms: f32, now_us: i64) -> f32 {
        let speed = if speed_ms.is_finite() {
            speed_ms.max(0.0)
        } else {
            0.0
        };
        let next = match self.shown {
            Some((shown, at)) if (target_m - shown).abs() <= SNAP_M => {
                let dt = ((now_us - at) as f32 / 1e6).max(0.0);
                self.lean_speed += (speed - self.lean_speed) * (1.0 - (-dt / LEAN_GLIDE_S).exp());
                let carried = shown + speed * dt;
                let closed = carried + (target_m - carried) * (1.0 - (-dt / GLIDE_S).exp());
                closed.max(shown)
            }
            _ => {
                self.lean_speed = speed;
                target_m
            }
        };
        self.shown = Some((next, now_us));
        next
    }

    /// The speed to lean the rider by: the ride's, eased like the distance.
    fn lean_speed(&self) -> f32 {
        self.lean_speed
    }
}

/// Everything living in the GL context. Created on realize, dropped on
/// unrealize — the context and everything in it go together.
struct GlState {
    gl: glow::Context,
    program: glow::Program,
    sky_program: glow::Program,
    finish_program: glow::Program,
    prop_program: glow::Program,
    hud_program: glow::Program,
    /// The HUD as last painted, and its size: replaced only when it changes.
    hud: Option<(glow::Texture, (i32, i32))>,
    /// Core profiles draw nothing without a vertex array bound, even for a
    /// triangle made up in the shader.
    empty_vao: glow::VertexArray,
    /// The land, the road and everything fixed to them.
    world: MeshGl,
    /// Each biome's horizon, by `Biome as usize`; empty where the ride has
    /// none of it.
    horizons: Vec<MeshGl>,
    clouds: MeshGl,
    rider_body: MeshGl,
    rider_leg: MeshGl,
    /// The ghost rider: the rider's shape in the ghost's colours.
    ghost_body: MeshGl,
    ghost_leg: MeshGl,
    /// One per prop shape: its mesh, and every prop of that shape.
    props: Vec<PropShape>,
    /// The scene is drawn here first, and the finishing pass draws it into
    /// GTK's framebuffer.
    target: Option<Target>,
    u: Uniforms,
    /// Whether the GL state GTK handed over has been logged — once per
    /// context, for the day a rider's screen shows only sky.
    reported: Cell<bool>,
}

/// A mesh on the GPU.
struct MeshGl {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    index_count: i32,
}

/// The offscreen framebuffer the scene is drawn into, at the view's size.
struct Target {
    fbo: glow::Framebuffer,
    colour: glow::Texture,
    depth: glow::Texture,
    size: (i32, i32),
}

/// One prop shape on the GPU: its mesh, and the props drawn with it.
struct PropShape {
    shape: Shape,
    mesh: MeshGl,
    instances: glow::Buffer,
    chunks: Vec<props::Chunk>,
    /// This frame's encounter animals of this shape: a few, sent anew each
    /// frame.
    live: glow::Buffer,
    live_count: i32,
}

type Uniform = Option<glow::UniformLocation>;

/// What the world and prop programs both read.
struct SceneUniforms {
    mvp: Uniform,
    model: Uniform,
    eye: Uniform,
    horizon: Uniform,
    zenith: Uniform,
    sun: Uniform,
    foam: Uniform,
    fog_start: Uniform,
    fog_density: Uniform,
    time: Uniform,
    orbit: Uniform,
    head: Uniform,
    hub: Uniform,
    legs: Uniform,
    sunlit: Uniform,
    shade: Uniform,
    rim: Uniform,
    haze: Uniform,
    patch_cool: Uniform,
    patch_warm: Uniform,
    crack: Uniform,
    alpha: Uniform,
}

impl SceneUniforms {
    fn of(gl: &glow::Context, p: glow::Program) -> Self {
        let at = |name: &str| unsafe { gl.get_uniform_location(p, name) };
        Self {
            mvp: at("u_mvp"),
            model: at("u_model"),
            eye: at("u_eye"),
            horizon: at("u_horizon"),
            zenith: at("u_zenith"),
            sun: at("u_sun"),
            foam: at("u_foam"),
            fog_start: at("u_fog_start"),
            fog_density: at("u_fog_density"),
            time: at("u_time"),
            orbit: at("u_orbit"),
            head: at("u_head"),
            hub: at("u_hub"),
            legs: at("u_legs"),
            sunlit: at("u_sunlit"),
            shade: at("u_shade"),
            rim: at("u_rim"),
            haze: at("u_haze"),
            patch_cool: at("u_patch_cool"),
            patch_warm: at("u_patch_warm"),
            crack: at("u_crack"),
            alpha: at("u_alpha"),
        }
    }

    /// Set every one for a frame. The model starts as the identity.
    unsafe fn set(&self, gl: &glow::Context, mvp: &Mat4, eye: &[f32; 3], sky: &Sky, time_s: f32) {
        let light = &palette::LIGHT;
        gl.uniform_3_f32_slice(self.sunlit.as_ref(), &light.sunlit);
        gl.uniform_3_f32_slice(self.shade.as_ref(), &light.shade);
        gl.uniform_3_f32_slice(self.rim.as_ref(), &light.rim);
        gl.uniform_3_f32_slice(self.haze.as_ref(), &light.haze);
        gl.uniform_3_f32_slice(self.patch_cool.as_ref(), &light.patch_cool);
        gl.uniform_3_f32_slice(self.patch_warm.as_ref(), &light.patch_warm);
        gl.uniform_3_f32_slice(self.crack.as_ref(), &light.crack);
        gl.uniform_1_f32(self.alpha.as_ref(), 1.0);
        gl.uniform_matrix_4_f32_slice(self.mvp.as_ref(), false, mvp);
        gl.uniform_matrix_4_f32_slice(self.model.as_ref(), false, &course3d::IDENTITY);
        gl.uniform_3_f32_slice(self.eye.as_ref(), eye);
        gl.uniform_3_f32_slice(self.horizon.as_ref(), &sky.horizon);
        gl.uniform_3_f32_slice(self.zenith.as_ref(), &sky.top);
        gl.uniform_3_f32_slice(self.sun.as_ref(), &SUN_DIR);
        gl.uniform_3_f32_slice(self.foam.as_ref(), &palette::BASE.foam);
        gl.uniform_1_f32(self.fog_start.as_ref(), biome::FOG_START_M);
        gl.uniform_1_f32(self.fog_density.as_ref(), sky.fog_density);
        gl.uniform_1_f32(self.time.as_ref(), time_s);
        gl.uniform_1_f32(self.orbit.as_ref(), props::ORBIT_M);
        gl.uniform_1_f32(self.head.as_ref(), crate::data::animals::HEAD_FROM);
        gl.uniform_2_f32(
            self.hub.as_ref(),
            crate::data::kit::SAIL_FROM,
            crate::data::kit::HUB_Y,
        );
        gl.uniform_1_f32(self.legs.as_ref(), crate::data::kit::LEGS_TOP);
    }
}

/// Every uniform the programs read.
struct Uniforms {
    scene: SceneUniforms,
    props: SceneUniforms,
    sky_horizon: Uniform,
    sky_zenith: Uniform,
    sky_glow: Uniform,
    p_reach: Uniform,
    finish_colour: Uniform,
    finish_corner: Uniform,
    hud_image: Uniform,
}

struct Inner {
    root: gtk::Overlay,
    area: gtk::GLArea,
    /// "Building the 3D view…", over the top left of the view.
    status: gtk::Label,
    route: RefCell<Option<Route>>,
    /// What the world around the route is made of.
    look: Cell<biome::WorldLook>,
    scene: RefCell<Option<CourseScene>>,
    /// Runs the scene's animal encounters as the rider rides.
    director: RefCell<Option<encounters::Director>>,
    /// When the view was made: the encounters' clock starts here.
    born_us: i64,
    /// The scene changed and has not been uploaded yet.
    needs_upload: Cell<bool>,
    gl: RefCell<Option<GlState>>,
    progress: Cell<Progress>,
    glide: Cell<Glide>,
    hud: RefCell<HudLayer>,
    /// "See Ride Summary", laid over the finished card's button row, and
    /// what it does — once.
    finish_btn: gtk::Button,
    on_finish: RefCell<Option<Box<dyn FnOnce()>>>,
    /// The ride being raced, and the ride's clock of riding time — seconds,
    /// and when they were reported — that moves it.
    ghost: RefCell<Option<Ghost>>,
    ride_clock: Cell<Option<(f32, i64)>>,
    /// The distance last drawn, so an unmoving camera is not redrawn.
    drawn_m: Cell<Option<f32>>,
    /// Bumped for every new route, so a build that finishes after the next
    /// route has been set is thrown away rather than shown.
    generation: Cell<u64>,
}

/// The fullscreen ride HUD, while the view carries one, and the finished
/// card once the rider is over the line — in any layout.
#[derive(Default)]
struct HudLayer {
    shown: bool,
    readings: Option<HudReadings>,
    finished: Option<FinishedCard>,
    /// The route's heights, for the profile band.
    profile: Vec<f32>,
    /// Made on first use: loading the font is for fullscreen, not every view.
    painter: Option<RideHud>,
    /// What the texture on the GPU shows, and for what size of view.
    painted: Option<(Painted, (i32, i32))>,
}

/// What the HUD texture was last painted with.
#[derive(Debug, Clone, PartialEq)]
enum Painted {
    Readings(HudReadings),
    Finished(FinishedCard),
}

/// The 3D road view. Cheap to clone; clones share one widget.
#[derive(Clone)]
pub struct CourseView {
    inner: Rc<Inner>,
}

impl Default for CourseView {
    fn default() -> Self {
        Self::new()
    }
}

impl CourseView {
    pub fn new() -> Self {
        let area = gtk::GLArea::builder()
            .has_depth_buffer(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.update_property(&[gtk::accessible::Property::Label(
            "The road ahead, seen from behind the rider",
        )]);
        let status = gtk::Label::builder()
            .css_classes(["caption", "osd"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .visible(false)
            .build();
        let root = gtk::Overlay::builder().child(&area).build();
        root.add_overlay(&status);
        let finish_btn = gtk::Button::builder()
            .label("See Ride Summary")
            .tooltip_text("Save the ride and see its summary")
            .css_classes(["pill", "suggested-action"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        root.add_overlay(&finish_btn);

        let inner = Rc::new(Inner {
            root: root.clone(),
            status,
            area: area.clone(),
            route: RefCell::new(None),
            look: Cell::new(biome::WorldLook::default()),
            scene: RefCell::new(None),
            director: RefCell::new(None),
            born_us: glib::monotonic_time(),
            needs_upload: Cell::new(false),
            gl: RefCell::new(None),
            progress: Cell::new(Progress::default()),
            glide: Cell::new(Glide::default()),
            hud: RefCell::new(HudLayer::default()),
            finish_btn: finish_btn.clone(),
            on_finish: RefCell::new(None),
            ghost: RefCell::new(None),
            ride_clock: Cell::new(None),
            drawn_m: Cell::new(None),
            generation: Cell::new(0),
        });

        // Handlers hold the inner state weakly: the area owns these closures,
        // and a strong capture would keep it alive forever (CLAUDE.md §2.4).
        let weak = Rc::downgrade(&inner);
        finish_btn.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.finish_btn.set_visible(false);
                // Taken before it runs: it may set a new route on this view.
                let done = inner.on_finish.borrow_mut().take();
                if let Some(done) = done {
                    done();
                }
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_resize(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.place_finish_button();
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_realize(move |area| {
            if let Some(inner) = weak.upgrade() {
                inner.realize(area);
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_unrealize(move |area| {
            if let Some(inner) = weak.upgrade() {
                inner.unrealize(area);
            }
        });
        let weak = Rc::downgrade(&inner);
        area.connect_render(move |area, _ctx| {
            if let Some(inner) = weak.upgrade() {
                inner.render(area);
            }
            glib::Propagation::Stop
        });
        // Runs only while the area is mapped, so a hidden view costs nothing.
        // Animals and clouds move, so a shown scene is drawn every frame; the
        // frame clock stops when the window is hidden or minimised.
        let weak = Rc::downgrade(&inner);
        area.add_tick_callback(move |area, _clock| {
            let Some(inner) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = glib::monotonic_time();
            let d = inner.progress.get().distance_at(now);
            let moved = inner
                .drawn_m
                .get()
                .is_none_or(|drawn| (d - drawn).abs() > REDRAW_EPSILON_M);
            if moved || inner.needs_upload.get() || inner.scene.borrow().is_some() {
                area.queue_render();
            }
            glib::ControlFlow::Continue
        });

        Self { inner }
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.inner.root
    }

    /// Show `route` looking as `look`, with the rider at its start. The world is
    /// built off the main thread; the view says so until it is up.
    pub fn set_route(&self, route: &Route, look: biome::WorldLook) {
        let inner = &self.inner;
        inner.generation.set(inner.generation.get() + 1);
        *inner.scene.borrow_mut() = None;
        *inner.director.borrow_mut() = None;
        inner.needs_upload.set(true);
        inner.progress.set(Progress::default());
        inner.glide.set(Glide::default());
        inner.drawn_m.set(None);
        *inner.route.borrow_mut() = Some(route.clone());
        inner.look.set(look);
        {
            let mut hud = inner.hud.borrow_mut();
            hud.profile = ride_hud::profile(route);
            hud.readings = None;
            hud.finished = None;
            hud.painted = None;
        }
        inner.finish_btn.set_visible(false);
        *inner.on_finish.borrow_mut() = None;
        *inner.ghost.borrow_mut() = None;
        inner.ride_clock.set(None);
        Inner::rebuild(inner);
    }

    /// Race `ghost` on this route, or nothing.
    pub fn set_ghost(&self, ghost: Option<Ghost>) {
        *self.inner.ghost.borrow_mut() = ghost;
        self.inner.area.queue_render();
    }

    /// The ride's riding time, in seconds — what the ghost rides by. Called
    /// once a tick; between ticks the ghost is carried on for at most a
    /// second, so a pause stops it with the rider.
    pub fn set_ride_clock(&self, secs: u32) {
        self.inner
            .ride_clock
            .set(Some((secs as f32, glib::monotonic_time())));
    }

    /// The rider is over the line: let them coast to a stop past the arch,
    /// lay `card` over the road, and run `then` — once — when they ask for
    /// the ride's summary.
    pub fn show_finished(&self, card: FinishedCard, then: impl FnOnce() + 'static) {
        let inner = &self.inner;
        let now = glib::monotonic_time();
        let progress = inner.progress.get();
        inner.progress.set(Progress {
            distance_m: progress.distance_at(now),
            speed_ms: progress.speed_at(now),
            at_us: Some(now),
            coast: true,
        });
        let tiles: Vec<String> = card
            .tiles()
            .iter()
            .map(|(caption, value, unit)| format!("{} {value} {unit}", caption.to_lowercase()))
            .collect();
        inner
            .finish_btn
            .update_property(&[gtk::accessible::Property::Description(&format!(
                "Finished {}: {}",
                card.route_name,
                tiles.join(", ")
            ))]);
        inner.hud.borrow_mut().finished = Some(card);
        *inner.on_finish.borrow_mut() = Some(Box::new(then));
        inner.place_finish_button();
        inner.finish_btn.set_visible(true);
        inner.finish_btn.grab_focus();
        inner.area.queue_render();
    }

    /// Whether the view is on screen: the finished card is for a rider
    /// watching the road.
    pub fn is_showing(&self) -> bool {
        self.inner.area.is_mapped()
    }

    /// Where the ride is: `distance_m` along the route, moving at `speed_ms`.
    /// Called once a tick by the ride loop.
    pub fn set_progress(&self, distance_m: f32, speed_ms: f32) {
        self.inner.progress.set(Progress {
            distance_m,
            speed_ms,
            at_us: Some(glib::monotonic_time()),
            coast: false,
        });
    }

    /// Show or hide the fullscreen ride HUD over the road.
    pub fn set_hud_shown(&self, shown: bool) {
        self.inner.hud.borrow_mut().shown = shown;
        self.inner.area.queue_render();
    }

    /// What the HUD shows. Called once a tick by the ride loop; the HUD is
    /// painted again only when this changes.
    pub fn set_hud(&self, readings: HudReadings) {
        self.inner.hud.borrow_mut().readings = Some(readings);
    }
}

impl Inner {
    /// Build the world from the route, off the main thread — a long route
    /// takes long enough to stall a frame.
    fn rebuild(this: &Rc<Self>) {
        let Some(route) = this.route.borrow().clone() else {
            return;
        };
        let generation = this.generation.get();
        let scenery = this.look.get().scenery;
        this.update_status();
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            let built = gtk::gio::spawn_blocking(move || CourseScene::build(&route, scenery)).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.generation.get() != generation {
                return;
            }
            match built {
                Ok(scene) => {
                    // With animals off there are no encounters at all.
                    *inner.director.borrow_mut() = scene
                        .as_ref()
                        .filter(|_| inner.look.get().animals)
                        .map(|s| encounters::Director::new(s.encounters.clone()));
                    *inner.scene.borrow_mut() = scene;
                    inner.needs_upload.set(true);
                    inner.area.queue_render();
                }
                Err(_) => tracing::error!("Building the 3D course panicked"),
            }
            inner.update_status();
        });
    }

    /// Lay the finished card's button on the card's button row.
    fn place_finish_button(&self) {
        let scale = self.area.scale_factor().max(1);
        let (w, h) = (self.area.width() * scale, self.area.height() * scale);
        let drop = ride_hud::finish_button_drop(w, h) / f64::from(scale);
        // Centred in what the margin leaves: half the margin moves it down.
        self.finish_btn.set_margin_top((2.0 * drop).round() as i32);
    }

    /// Say the world is being built, until it is.
    fn update_status(&self) {
        let building = self.scene.borrow().is_none()
            && self
                .route
                .borrow()
                .as_ref()
                .is_some_and(|r| r.points.len() >= 2);
        self.status.set_label(BUILDING);
        self.status.set_visible(building);
    }

    fn realize(&self, area: &gtk::GLArea) {
        area.make_current();
        if let Some(e) = area.error() {
            tracing::error!("3D course view has no GL context: {e}");
            return;
        }
        let Some(get_proc) = egl_get_proc_address() else {
            area.set_error(Some(&glib::Error::new(
                gtk::gdk::GLError::NotAvailable,
                "Could not load OpenGL",
            )));
            return;
        };
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                let Ok(c) = CString::new(name) else {
                    return std::ptr::null();
                };
                get_proc(c.as_ptr())
            })
        };
        let es = area.context().is_some_and(|c| c.uses_es());
        match GlState::new(gl, es) {
            Ok(state) => {
                *self.gl.borrow_mut() = Some(state);
                self.needs_upload.set(true);
            }
            Err(e) => {
                tracing::error!("3D course view could not start: {e}");
                area.set_error(Some(&glib::Error::new(
                    gtk::gdk::GLError::CompilationFailed,
                    "The 3D view could not start on this graphics card",
                )));
            }
        }
    }

    fn unrealize(&self, area: &gtk::GLArea) {
        area.make_current();
        if let Some(state) = self.gl.borrow_mut().take() {
            state.destroy();
        }
    }

    fn render(&self, area: &gtk::GLArea) {
        let mut gl_slot = self.gl.borrow_mut();
        let Some(state) = gl_slot.as_mut() else {
            return;
        };
        if self.needs_upload.get() {
            self.needs_upload.set(false);
            let scene = self.scene.borrow();
            state.upload(scene.as_ref(), self.look.get().animals);
        }

        let scale = area.scale_factor();
        let (w, h) = (area.width() * scale, area.height() * scale);
        let now = glib::monotonic_time();
        let progress = self.progress.get();
        let speed = progress.speed_at(now);
        let mut glide = self.glide.get();
        let distance = glide.follow(progress.distance_at(now), speed, now);
        self.glide.set(glide);
        let speed = glide.lean_speed();
        let (pose, sky) = match self.scene.borrow().as_ref() {
            // The run-out is the route's own road up to the line, so the
            // rider rides the same road either way; past it, they coast on.
            Some(s) => (
                course3d::rider_pose(&s.run_out, distance, speed)
                    .map(|r| (course3d::chase_camera(&s.run_out, &r), r)),
                s.sky_at(distance),
            ),
            None => (None, Sky::default()),
        };
        // Seconds, wrapped every hour so the animals' motion keeps its
        // precision.
        let time_s = (now / 1_000 % 3_600_000) as f32 / 1_000.0;

        // The encounters run on their own clock, which never wraps.
        let clock_s = ((now - self.born_us) as f64 / 1e6) as f32;
        let live = match (
            self.scene.borrow().as_ref(),
            self.director.borrow_mut().as_mut(),
        ) {
            (Some(s), Some(d)) => d.update(&s.road, distance, speed, clock_s),
            _ => Vec::new(),
        };
        let ghost = match (self.ghost.borrow().as_ref(), self.scene.borrow().as_ref()) {
            (Some(g), Some(s)) => {
                // At the start line until the ride's clock starts.
                let secs = self.ride_clock.get().map_or(0.0, |(secs, at)| {
                    secs + ((now - at) as f32 / 1e6).clamp(0.0, MAX_EXTRAPOLATE_S)
                });
                let d = g.distance_at(secs);
                let v = g.distance_at(secs + 0.5) - g.distance_at(secs - 0.5);
                course3d::rider_pose_in_lane(&s.run_out, d, v, course3d::GHOST_OFFSET_M)
            }
            _ => None,
        };
        state.draw(w, h, pose, ghost, sky, time_s, &live);
        self.drawn_m.set(pose.map(|_| distance));

        let mut hud = self.hud.borrow_mut();
        let HudLayer {
            shown,
            readings,
            finished,
            profile,
            painter,
            painted,
        } = &mut *hud;
        // The finished card in any layout; else the HUD, when it is shown.
        let want = match (finished.as_ref(), *shown, readings.as_ref()) {
            (Some(card), _, _) => Some(Painted::Finished(card.clone())),
            (None, true, Some(r)) => Some(Painted::Readings(r.clone())),
            _ => None,
        };
        if let Some(want) = want {
            let fresh = painted
                .as_ref()
                .is_some_and(|(p, size)| *p == want && *size == (w, h));
            if !fresh {
                let painter = painter.get_or_insert_with(RideHud::new);
                let image = match &want {
                    Painted::Readings(r) => painter.render(w, h, r, profile),
                    Painted::Finished(card) => painter.render_finished(w, h, card),
                };
                if let Some(mut image) = image {
                    state.upload_hud(&mut image);
                    *painted = Some((want, (w, h)));
                }
            }
            state.draw_hud(w, h);
        }
    }
}

impl MeshGl {
    /// `mesh` on the GPU, with the world's vertex layout.
    unsafe fn new(gl: &glow::Context, mesh: &Mesh) -> Result<Self, String> {
        let vao = gl.create_vertex_array()?;
        let vbo = gl.create_buffer()?;
        let ebo = gl.create_buffer()?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
        let stride = std::mem::size_of::<course3d::Vertex>() as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 12);
        gl.bind_vertex_array(None);
        let mut this = Self {
            vao,
            vbo,
            ebo,
            index_count: 0,
        };
        this.replace(gl, Some(mesh));
        Ok(this)
    }

    /// Replace what is on the GPU with `mesh`, or with nothing.
    fn replace(&mut self, gl: &glow::Context, mesh: Option<&Mesh>) {
        let (vertices, indices) = match mesh {
            Some(m) => (vertex_bytes(m), index_bytes(m)),
            None => (Vec::new(), Vec::new()),
        };
        unsafe {
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &vertices, glow::STATIC_DRAW);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ebo));
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, &indices, glow::STATIC_DRAW);
            gl.bind_vertex_array(None);
        }
        self.index_count = mesh.map_or(0, |m| m.indices.len() as i32);
    }

    unsafe fn draw(&self, gl: &glow::Context) {
        if self.index_count == 0 {
            return;
        }
        gl.bind_vertex_array(Some(self.vao));
        gl.draw_elements(glow::TRIANGLES, self.index_count, glow::UNSIGNED_INT, 0);
        gl.bind_vertex_array(None);
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
            gl.delete_buffer(self.ebo);
        }
    }
}

impl GlState {
    fn new(gl: glow::Context, es: bool) -> Result<Self, String> {
        let header = if es {
            "#version 300 es\nprecision highp float;\n"
        } else {
            "#version 330 core\n"
        };
        unsafe {
            let program = link(
                &gl,
                header,
                VERTEX_SHADER,
                FRAGMENT_SHADER,
                &[(0, "a_pos"), (1, "a_colour")],
            )?;
            let sky_program = link(
                &gl,
                header,
                FULLSCREEN_VERTEX_SHADER,
                SKY_FRAGMENT_SHADER,
                &[],
            )?;
            let finish_program = link(
                &gl,
                header,
                FULLSCREEN_VERTEX_SHADER,
                FINISH_FRAGMENT_SHADER,
                &[],
            )?;
            let hud_program = link(
                &gl,
                header,
                FULLSCREEN_VERTEX_SHADER,
                HUD_FRAGMENT_SHADER,
                &[],
            )?;
            let prop_program = link(
                &gl,
                header,
                PROP_VERTEX_SHADER,
                FRAGMENT_SHADER,
                &[
                    (0, "a_pos"),
                    (1, "a_colour"),
                    (3, "i_pos"),
                    (4, "i_size"),
                    (5, "i_colour"),
                    (6, "i_motion"),
                ],
            )?;
            let empty_vao = gl.create_vertex_array()?;
            let empty = Mesh::default();
            let world = MeshGl::new(&gl, &empty)?;
            let horizons = (0..biome::COUNT)
                .map(|_| MeshGl::new(&gl, &empty))
                .collect::<Result<Vec<_>, _>>()?;
            let clouds = MeshGl::new(&gl, &empty)?;
            let rider_body = MeshGl::new(&gl, &rider::body(&rider::Avatar::default()))?;
            let rider_leg = MeshGl::new(&gl, &rider::leg(&rider::Avatar::default()))?;
            let ghost_body = MeshGl::new(&gl, &rider::body(&rider::Avatar::ghost()))?;
            let ghost_leg = MeshGl::new(&gl, &rider::leg(&rider::Avatar::ghost()))?;
            let props = Shape::ALL
                .iter()
                .map(|&shape| PropShape::new(&gl, shape))
                .collect::<Result<Vec<_>, _>>()?;

            let at = |p: glow::Program, name: &str| gl.get_uniform_location(p, name);
            let u = Uniforms {
                scene: SceneUniforms::of(&gl, program),
                props: SceneUniforms::of(&gl, prop_program),
                sky_horizon: at(sky_program, "u_horizon"),
                sky_zenith: at(sky_program, "u_zenith"),
                sky_glow: at(sky_program, "u_glow"),
                p_reach: at(prop_program, "u_reach"),
                finish_colour: at(finish_program, "u_colour"),
                finish_corner: at(finish_program, "u_corner"),
                hud_image: at(hud_program, "u_hud"),
            };
            Ok(Self {
                gl,
                program,
                sky_program,
                finish_program,
                prop_program,
                hud_program,
                hud: None,
                empty_vao,
                world,
                horizons,
                clouds,
                rider_body,
                rider_leg,
                ghost_body,
                ghost_leg,
                props,
                target: None,
                u,
                reported: Cell::new(false),
            })
        }
    }

    /// Replace what is on the GPU with `scene`, or with nothing.
    /// `animals` false leaves every animal out: herds and birds.
    fn upload(&mut self, scene: Option<&CourseScene>, animals: bool) {
        let batches = scene
            .map(|s| {
                let kept: Vec<Prop> = s
                    .props
                    .iter()
                    .filter(|p| animals || !p.shape.is_animal())
                    .copied()
                    .collect();
                props::batches(&kept)
            })
            .unwrap_or_default();
        for shape in &mut self.props {
            let batch = batches.iter().find(|b| b.shape == shape.shape);
            let data: Vec<u8> = batch
                .map(|b| b.instances.iter().flat_map(|f| f.to_ne_bytes()).collect())
                .unwrap_or_default();
            unsafe {
                self.gl
                    .bind_buffer(glow::ARRAY_BUFFER, Some(shape.instances));
                self.gl
                    .buffer_data_u8_slice(glow::ARRAY_BUFFER, &data, glow::STATIC_DRAW);
                self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
            shape.chunks = batch.map(|b| b.chunks.clone()).unwrap_or_default();
        }
        self.world.replace(&self.gl, scene.map(|s| &s.mesh));
        for (k, horizon) in self.horizons.iter_mut().enumerate() {
            let mesh = scene.and_then(|s| {
                s.horizons
                    .iter()
                    .find(|(b, _)| *b as usize == k)
                    .map(|(_, m)| m)
            });
            horizon.replace(&self.gl, mesh);
        }
        self.clouds.replace(&self.gl, scene.map(|s| &s.clouds));
    }

    /// Send this frame's encounter animals, shape by shape. A frame with
    /// none sends nothing.
    fn upload_live(&mut self, live: &[Prop]) {
        for shape in &mut self.props {
            let mine = live.iter().filter(|p| p.shape == shape.shape);
            let count = mine.clone().count() as i32;
            if count == 0 && shape.live_count == 0 {
                continue;
            }
            shape.live_count = count;
            if count == 0 {
                continue;
            }
            let bytes: Vec<u8> = props::instances(mine)
                .iter()
                .flat_map(|f| f.to_ne_bytes())
                .collect();
            unsafe {
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(shape.live));
                self.gl
                    .buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STREAM_DRAW);
                self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        }
    }

    /// The offscreen framebuffer at `width` × `height`, made or remade as
    /// the view's size changes.
    fn target(&mut self, width: i32, height: i32) -> Result<&Target, String> {
        let size = (width.max(1), height.max(1));
        if self.target.as_ref().is_some_and(|t| t.size != size) {
            if let Some(old) = self.target.take() {
                old.destroy(&self.gl);
            }
        }
        if self.target.is_none() {
            self.target = Some(unsafe { Target::new(&self.gl, size)? });
        }
        self.target.as_ref().ok_or_else(|| "no target".to_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        width: i32,
        height: i32,
        pose: Option<(CameraPose, RiderPose)>,
        ghost: Option<RiderPose>,
        sky: Sky,
        time_s: f32,
        live: &[Prop],
    ) {
        self.upload_live(live);
        // Logged on the first frame that has a scene to draw, which is the
        // one that matters if the rider sees only sky.
        let first = pose.is_some() && self.world.index_count > 0 && !self.reported.replace(true);
        if first {
            self.report_state(width, height, true);
        }
        // Where GTK wants the picture: its framebuffer, bound now.
        let screen = unsafe {
            let id = self.gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            std::num::NonZeroU32::new(id as u32).map(glow::NativeFramebuffer)
        };
        let target = match self.target(width, height) {
            Ok(t) => (t.fbo, t.colour),
            Err(e) => {
                tracing::error!("3D view could not make its framebuffer: {e}");
                return;
            }
        };
        let (fbo, colour_tex) = target;
        let gl = &self.gl;
        let u = &self.u;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.draw_buffers(&[glow::COLOR_ATTACHMENT0]);
            // GTK renders with the same context and leaves its own state
            // behind. Nothing here may be assumed: set everything the scene
            // relies on, every frame.
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::STENCIL_TEST);
            // The sky goes down first, under everything: GTK leaves the depth
            // test on, and a depth-tested sky vanishes into the haze colour.
            gl.disable(glow::DEPTH_TEST);
            gl.depth_mask(true);
            gl.color_mask(true, true, true, true);
            gl.depth_range_f32(0.0, 1.0);
            // GTK's renderer clears depth to its own value; left at 0, every
            // fragment of the scene fails the depth test and only the sky
            // (drawn without it) shows. That is exactly what the rider saw.
            gl.clear_depth_f32(1.0);
            gl.front_face(glow::CCW);
            gl.viewport(0, 0, width, height);
            gl.clear_color(sky.horizon[0], sky.horizon[1], sky.horizon[2], 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.use_program(Some(self.sky_program));
            gl.uniform_3_f32_slice(u.sky_horizon.as_ref(), &sky.horizon);
            gl.uniform_3_f32_slice(u.sky_zenith.as_ref(), &sky.top);
            gl.uniform_3_f32_slice(u.sky_glow.as_ref(), &palette::LIGHT.horizon_glow);
            gl.bind_vertex_array(Some(self.empty_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            if let (Some((camera, rider_pose)), true) = (pose, self.world.index_count > 0) {
                let aspect = width as f32 / height.max(1) as f32;
                let mvp = course3d::mul4(
                    &course3d::perspective(FOV_DEG, aspect, NEAR_M, FAR_M),
                    &course3d::look_at(camera.eye, camera.target),
                );
                gl.enable(glow::DEPTH_TEST);
                gl.depth_func(glow::LEQUAL);
                gl.use_program(Some(self.program));
                u.scene.set(gl, &mvp, &camera.eye, &sky, time_s);

                // The horizon, around the eye: mountains, and the clouds
                // drifting slowly round. Then depth is cleared, so the world
                // always stands in front of it.
                let around = course3d::translation(camera.eye);
                // Each biome's horizon at its height: on a mixed ride one
                // sinks into the haze as the next rises out of it.
                for (k, horizon) in self.horizons.iter().enumerate() {
                    let rise = sky.backdrop[k].clamp(0.0, 1.0);
                    if rise < 0.01 || horizon.index_count == 0 {
                        continue;
                    }
                    let mut grow = course3d::IDENTITY;
                    grow[5] = rise;
                    grow[13] = crate::data::sky::FOOT_M * (1.0 - rise);
                    let model = course3d::mul4(&around, &grow);
                    gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &model);
                    horizon.draw(gl);
                }
                gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &around);
                let drift = course3d::mul4(&around, &course3d::yaw(time_s * CLOUD_DRIFT));
                gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &drift);
                self.clouds.draw(gl);
                gl.clear(glow::DEPTH_BUFFER_BIT);

                gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &course3d::IDENTITY);
                self.world.draw(gl);

                // The rider, and their legs turning with the cranks.
                draw_rider(
                    gl,
                    &u.scene.model,
                    &rider_pose,
                    &self.rider_body,
                    &self.rider_leg,
                );

                gl.use_program(Some(self.prop_program));
                u.props.set(gl, &mvp, &camera.eye, &sky, time_s);
                // Which way the camera looks, flat, and how wide: chunks
                // wholly behind it or off to a side are not drawn. A little
                // wider than the view, for the camera's pitch.
                let tan_half = (FOV_DEG.to_radians() / 2.0).tan() * aspect * 1.15;
                let forward = {
                    let (fx, fz) = (
                        camera.target[0] - camera.eye[0],
                        camera.target[2] - camera.eye[2],
                    );
                    let len = fx.hypot(fz);
                    if len > 1e-3 {
                        (fx / len, fz / len)
                    } else {
                        (f32::NAN, f32::NAN)
                    }
                };
                for shape in &self.props {
                    let reach = shape.shape.reach_m();
                    let (gone_from, gone) = match reach {
                        Some(r) => (0.75 * r, r),
                        None => (f32::MAX / 2.0, f32::MAX),
                    };
                    gl.uniform_2_f32(u.p_reach.as_ref(), gone_from, gone);
                    shape.draw(
                        gl,
                        camera.eye,
                        forward,
                        tan_half,
                        reach.unwrap_or(PROP_DRAW_M),
                    );
                    shape.draw_live(gl);
                }
                // The ghost, last and see-through. Its depth goes down first
                // on its own, so where its legs pass behind its body only the
                // nearer shows, as on a solid rider seen through glass.
                if let Some(g) = ghost {
                    gl.use_program(Some(self.program));
                    gl.color_mask(false, false, false, false);
                    draw_rider(gl, &u.scene.model, &g, &self.ghost_body, &self.ghost_leg);
                    gl.color_mask(true, true, true, true);
                    gl.depth_mask(false);
                    gl.enable(glow::BLEND);
                    gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                    gl.uniform_1_f32(u.scene.alpha.as_ref(), GHOST_ALPHA);
                    draw_rider(gl, &u.scene.model, &g, &self.ghost_body, &self.ghost_leg);
                    gl.uniform_1_f32(u.scene.alpha.as_ref(), 1.0);
                    gl.disable(glow::BLEND);
                    gl.depth_mask(true);
                }
                if first {
                    let err = gl.get_error();
                    tracing::info!(
                        "3D view first draw: {} indices, GL error 0x{err:X}",
                        self.world.index_count
                    );
                }
                gl.disable(glow::DEPTH_TEST);
            }

            // Into GTK's framebuffer, finished.
            gl.bind_framebuffer(glow::FRAMEBUFFER, screen);
            gl.viewport(0, 0, width, height);
            gl.use_program(Some(self.finish_program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(colour_tex));
            gl.uniform_1_i32(u.finish_colour.as_ref(), 0);
            gl.uniform_3_f32_slice(u.finish_corner.as_ref(), &palette::LIGHT.corner);
            gl.bind_vertex_array(Some(self.empty_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.use_program(None);
        }
    }

    /// Put a freshly painted HUD on the GPU, reusing the texture while the
    /// view keeps its size.
    fn upload_hud(&mut self, image: &mut cairo::ImageSurface) {
        let (w, h, stride) = (image.width(), image.height(), image.stride());
        let Ok(pixels) = image.data() else {
            return;
        };
        let gl = &self.gl;
        unsafe {
            let tex = match self.hud {
                Some((tex, size)) if size == (w, h) => tex,
                stale => {
                    if let Some((old, _)) = stale {
                        gl.delete_texture(old);
                    }
                    let Ok(tex) = gl.create_texture() else {
                        tracing::error!("3D view could not make the HUD's texture");
                        self.hud = None;
                        return;
                    };
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    for (key, value) in [
                        (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                        (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                        (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                        (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                    ] {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, key, value as i32);
                    }
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        w,
                        h,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None),
                    );
                    tex
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, stride / 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&pixels)),
            );
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            self.hud = Some((tex, (w, h)));
        }
    }

    /// Lay the HUD over the frame just drawn, in GTK's framebuffer.
    fn draw_hud(&self, width: i32, height: i32) {
        let Some((tex, _)) = self.hud else {
            return;
        };
        let gl = &self.gl;
        unsafe {
            gl.viewport(0, 0, width, height);
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            // Cairo's alpha is already multiplied in.
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.use_program(Some(self.hud_program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(self.u.hud_image.as_ref(), 0);
            gl.bind_vertex_array(Some(self.empty_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
            gl.disable(glow::BLEND);
        }
    }

    /// Log what GTK hands the view: version, the framebuffer it draws into,
    /// and the state the scene depends on. No personal data — GL only.
    fn report_state(&self, width: i32, height: i32, has_pose: bool) {
        let gl = &self.gl;
        unsafe {
            let version = gl.get_parameter_string(glow::VERSION);
            let fbo = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
            let depth_kind = gl.get_framebuffer_attachment_parameter_i32(
                glow::DRAW_FRAMEBUFFER,
                if fbo == 0 {
                    glow::DEPTH
                } else {
                    glow::DEPTH_ATTACHMENT
                },
                glow::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
            );
            let depth_bits = if depth_kind != 0 {
                gl.get_framebuffer_attachment_parameter_i32(
                    glow::DRAW_FRAMEBUFFER,
                    if fbo == 0 {
                        glow::DEPTH
                    } else {
                        glow::DEPTH_ATTACHMENT
                    },
                    glow::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE,
                )
            } else {
                0
            };
            tracing::info!(
                "3D view GL: {version}; fbo {fbo}, depth attachment 0x{depth_kind:X} ({depth_bits} bits); \
                 {width}×{height}; scissor {} blend {} cull {} depth-test {} depth-write {} \
                 depth-clear {} depth-func 0x{:X}; pose {has_pose}",
                gl.is_enabled(glow::SCISSOR_TEST),
                gl.is_enabled(glow::BLEND),
                gl.is_enabled(glow::CULL_FACE),
                gl.is_enabled(glow::DEPTH_TEST),
                gl.get_parameter_i32(glow::DEPTH_WRITEMASK),
                gl.get_parameter_f32(glow::DEPTH_CLEAR_VALUE),
                gl.get_parameter_i32(glow::DEPTH_FUNC),
            );
            let err = gl.get_error();
            if err != glow::NO_ERROR {
                tracing::info!("3D view GL state query raised 0x{err:X}");
            }
        }
    }

    fn destroy(mut self) {
        if let Some(target) = self.target.take() {
            target.destroy(&self.gl);
        }
        for mesh in [
            &self.world,
            &self.clouds,
            &self.rider_body,
            &self.rider_leg,
            &self.ghost_body,
            &self.ghost_leg,
        ] {
            mesh.destroy(&self.gl);
        }
        for horizon in &self.horizons {
            horizon.destroy(&self.gl);
        }
        for shape in &self.props {
            shape.mesh.destroy(&self.gl);
            unsafe {
                self.gl.delete_buffer(shape.instances);
                self.gl.delete_buffer(shape.live);
            }
        }
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.sky_program);
            self.gl.delete_program(self.finish_program);
            self.gl.delete_program(self.prop_program);
            self.gl.delete_program(self.hud_program);
            if let Some((tex, _)) = self.hud.take() {
                self.gl.delete_texture(tex);
            }
            self.gl.delete_vertex_array(self.empty_vao);
        }
    }
}

impl PropShape {
    /// The shape's mesh on the GPU, with an empty buffer for its props.
    unsafe fn new(gl: &glow::Context, shape: Shape) -> Result<Self, String> {
        let mesh = MeshGl::new(gl, &shape.mesh())?;
        let instances = gl.create_buffer()?;
        let live = gl.create_buffer()?;
        gl.bind_vertex_array(Some(mesh.vao));
        for slot in [3, 4, 5, 6] {
            gl.enable_vertex_attrib_array(slot);
            gl.vertex_attrib_divisor(slot, 1);
        }
        gl.bind_vertex_array(None);
        Ok(Self {
            shape,
            mesh,
            instances,
            chunks: Vec::new(),
            live,
            live_count: 0,
        })
    }

    /// Draw this frame's encounter animals of this shape.
    unsafe fn draw_live(&self, gl: &glow::Context) {
        if self.live_count == 0 || self.mesh.index_count == 0 {
            return;
        }
        let stride = (props::INSTANCE_FLOATS * 4) as i32;
        gl.bind_vertex_array(Some(self.mesh.vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.live));
        gl.vertex_attrib_pointer_f32(3, 3, glow::FLOAT, false, stride, 0);
        gl.vertex_attrib_pointer_f32(4, 2, glow::FLOAT, false, stride, 12);
        gl.vertex_attrib_pointer_f32(5, 3, glow::FLOAT, false, stride, 20);
        gl.vertex_attrib_pointer_f32(6, 2, glow::FLOAT, false, stride, 32);
        gl.draw_elements_instanced(
            glow::TRIANGLES,
            self.mesh.index_count,
            glow::UNSIGNED_INT,
            0,
            self.live_count,
        );
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);
    }

    /// Draw the props of every chunk within reach of the eye. Each chunk's
    /// run is reached by pointing the per-prop attributes at its start: GL
    /// ES 3.0 has no base instance.
    unsafe fn draw(
        &self,
        gl: &glow::Context,
        eye: [f32; 3],
        forward: (f32, f32),
        tan_half: f32,
        reach_m: f32,
    ) {
        if self.chunks.is_empty() || self.mesh.index_count == 0 {
            return;
        }
        let stride = (props::INSTANCE_FLOATS * 4) as i32;
        gl.bind_vertex_array(Some(self.mesh.vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.instances));
        for chunk in &self.chunks {
            if !props::in_view(chunk, eye, forward, tan_half, reach_m) {
                continue;
            }
            let base = chunk.start as i32 * stride;
            gl.vertex_attrib_pointer_f32(3, 3, glow::FLOAT, false, stride, base);
            gl.vertex_attrib_pointer_f32(4, 2, glow::FLOAT, false, stride, base + 12);
            gl.vertex_attrib_pointer_f32(5, 3, glow::FLOAT, false, stride, base + 20);
            gl.vertex_attrib_pointer_f32(6, 2, glow::FLOAT, false, stride, base + 32);
            gl.draw_elements_instanced(
                glow::TRIANGLES,
                self.mesh.index_count,
                glow::UNSIGNED_INT,
                0,
                chunk.count as i32,
            );
        }
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);
    }
}

impl Target {
    /// A framebuffer with a colour texture, which the finishing pass reads,
    /// and a depth texture, both `size`.
    unsafe fn new(gl: &glow::Context, size: (i32, i32)) -> Result<Self, String> {
        let texture = |internal: u32, format: u32, kind: u32| -> Result<glow::Texture, String> {
            let t = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                size.0,
                size.1,
                0,
                format,
                kind,
                glow::PixelUnpackData::Slice(None),
            );
            for (k, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, k, v as i32);
            }
            Ok(t)
        };
        let colour = texture(glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE)?;
        let depth = texture(
            glow::DEPTH_COMPONENT24,
            glow::DEPTH_COMPONENT,
            glow::UNSIGNED_INT,
        )?;
        gl.bind_texture(glow::TEXTURE_2D, None);
        let fbo = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        for (attachment, t) in [
            (glow::COLOR_ATTACHMENT0, colour),
            (glow::DEPTH_ATTACHMENT, depth),
        ] {
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, attachment, glow::TEXTURE_2D, Some(t), 0);
        }
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let target = Self {
            fbo,
            colour,
            depth,
            size,
        };
        if status != glow::FRAMEBUFFER_COMPLETE {
            target.destroy(gl);
            return Err(format!("framebuffer incomplete: 0x{status:X}"));
        }
        Ok(target)
    }

    fn destroy(self, gl: &glow::Context) {
        unsafe {
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.colour);
            gl.delete_texture(self.depth);
        }
    }
}

/// A rider at `pose` — `body`, and `leg` twice, turning with the cranks — with
/// the world program in use.
unsafe fn draw_rider(
    gl: &glow::Context,
    model: &Uniform,
    pose: &RiderPose,
    body: &MeshGl,
    leg: &MeshGl,
) {
    let at = course3d::rider_model(pose);
    gl.uniform_matrix_4_f32_slice(model.as_ref(), false, &at);
    body.draw(gl);
    for side in [-1.0f32, 1.0] {
        let (swing, lift) = rider::leg_pose(pose.crank, side);
        let hip = course3d::translation([
            rider::HIP[0],
            rider::HIP[1] + lift,
            rider::HIP[2] + side * rider::LEG_Z,
        ]);
        let m = course3d::mul4(&at, &course3d::mul4(&hip, &course3d::pitch(swing)));
        gl.uniform_matrix_4_f32_slice(model.as_ref(), false, &m);
        leg.draw(gl);
    }
}

/// Compile and link one program, binding `attributes` to their slots.
unsafe fn link(
    gl: &glow::Context,
    header: &str,
    vertex: &str,
    fragment: &str,
    attributes: &[(u32, &str)],
) -> Result<glow::Program, String> {
    let program = gl.create_program()?;
    let mut shaders = Vec::new();
    for (kind, src) in [
        (glow::VERTEX_SHADER, vertex),
        (glow::FRAGMENT_SHADER, fragment),
    ] {
        let shader = gl.create_shader(kind)?;
        gl.shader_source(shader, &format!("{header}{src}"));
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            return Err(gl.get_shader_info_log(shader));
        }
        gl.attach_shader(program, shader);
        shaders.push(shader);
    }
    for &(slot, name) in attributes {
        gl.bind_attrib_location(program, slot, name);
    }
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        return Err(gl.get_program_info_log(program));
    }
    for shader in shaders {
        gl.detach_shader(program, shader);
        gl.delete_shader(shader);
    }
    Ok(program)
}

fn vertex_bytes(mesh: &Mesh) -> Vec<u8> {
    mesh.vertices
        .iter()
        .flat_map(|v| v.pos.iter().chain(v.colour.iter()))
        .flat_map(|f| f.to_ne_bytes())
        .collect()
}

fn index_bytes(mesh: &Mesh) -> Vec<u8> {
    mesh.indices.iter().flat_map(|i| i.to_ne_bytes()).collect()
}

type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;

/// `eglGetProcAddress`, loaded once and kept for the life of the process.
fn egl_get_proc_address() -> Option<GetProcAddress> {
    static LOADER: OnceLock<Option<(libloading::Library, GetProcAddress)>> = OnceLock::new();
    LOADER
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libEGL.so.1")
                .map_err(|e| tracing::error!("Could not load libEGL: {e}"))
                .ok()?;
            let f: GetProcAddress = *lib
                .get::<GetProcAddress>(b"eglGetProcAddress\0")
                .map_err(|e| tracing::error!("libEGL has no eglGetProcAddress: {e}"))
                .ok()?;
            Some((lib, f))
        })
        .as_ref()
        .map(|(_, f)| *f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words GLSL keeps for itself — in OpenGL ES 3.0, which GTK may hand
    /// the view, and in desktop GL — that a shader may not name anything.
    const RESERVED: &[&str] = &[
        "attribute",
        "varying",
        "coherent",
        "volatile",
        "restrict",
        "readonly",
        "writeonly",
        "resource",
        "atomic_uint",
        "noperspective",
        "patch",
        "sample",
        "subroutine",
        "common",
        "partition",
        "active",
        "asm",
        "class",
        "union",
        "enum",
        "typedef",
        "template",
        "this",
        "goto",
        "inline",
        "noinline",
        "public",
        "static",
        "extern",
        "external",
        "interface",
        "long",
        "short",
        "double",
        "half",
        "fixed",
        "unsigned",
        "superp",
        "input",
        "output",
        "filter",
        "sizeof",
        "cast",
        "namespace",
        "using",
        "texture2D",
        "buffer",
        "shared",
        "precise",
        "invariant",
        "centroid",
        "smooth",
        "layout",
    ];

    #[test]
    fn should_name_nothing_in_a_shader_with_a_word_glsl_keeps_for_itself() {
        // Mesa's desktop compiler let `float patch` through offscreen; the
        // context GTK gave the rider refused it, and the view never started.
        // `layout`, `smooth`, `invariant` and `centroid` are keywords a
        // shader may use as such, but never as a name.
        let shaders = [
            ("VERTEX", VERTEX_SHADER),
            ("PROP_VERTEX", PROP_VERTEX_SHADER),
            ("FRAGMENT", FRAGMENT_SHADER),
            ("FULLSCREEN_VERTEX", FULLSCREEN_VERTEX_SHADER),
            ("HUD_FRAGMENT", HUD_FRAGMENT_SHADER),
            ("SKY_FRAGMENT", SKY_FRAGMENT_SHADER),
            ("FINISH_FRAGMENT", FINISH_FRAGMENT_SHADER),
        ];
        for (name, src) in shaders {
            let code: String = src
                .lines()
                .map(|l| l.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            let words = code
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .collect::<Vec<_>>();
            for (k, w) in words.iter().enumerate() {
                if !RESERVED.contains(w) {
                    continue;
                }
                // `layout(location = …)` is the one use the shaders make.
                let used_as_keyword = *w == "layout" && code.contains("layout(location");
                assert!(used_as_keyword, "{name} names something `{w}` (word {k})");
            }
        }
    }

    #[test]
    fn should_coast_through_the_line_to_a_stop() {
        // Over the line the ride stops reporting; the rider rolls on,
        // slowing, and comes to rest — not a second later in a lurch.
        let p = Progress {
            distance_m: 1_000.0,
            speed_ms: 10.0,
            at_us: Some(0),
            coast: true,
        };
        let after = |s: f32| 1_000.0 + 10.0 * COAST_S * (1.0 - (-s / COAST_S).exp());
        assert!((p.distance_at(1_000_000) - after(1.0)).abs() < 1e-3);
        assert!((p.distance_at(3_000_000) - after(3.0)).abs() < 1e-3);
        // At rest a little over the coasting length on, and staying there.
        assert!((p.distance_at(120_000_000) - (1_000.0 + 10.0 * COAST_S)).abs() < 1e-2);
        assert!(p.speed_at(1_000_000) < 10.0 && p.speed_at(1_000_000) > 5.0);
        assert!(p.speed_at(120_000_000) < 0.01);
        let back = Progress {
            speed_ms: -4.0,
            ..p
        };
        assert_eq!(back.distance_at(2_000_000), 1_000.0);
    }

    #[test]
    fn should_stay_put_before_the_first_report() {
        let p = Progress::default();
        assert_eq!(p.distance_at(5_000_000), 0.0);
    }

    #[test]
    fn should_carry_the_rider_on_between_reports() {
        let p = Progress {
            distance_m: 100.0,
            speed_ms: 8.0,
            at_us: Some(1_000_000),
            coast: false,
        };
        assert_eq!(p.distance_at(1_500_000), 104.0);
    }

    #[test]
    fn should_stop_a_second_after_the_last_report() {
        // A paused ride stops reporting; the view must not drive on alone.
        let p = Progress {
            distance_m: 100.0,
            speed_ms: 8.0,
            at_us: Some(1_000_000),
            coast: false,
        };
        assert_eq!(p.distance_at(2_000_000), 108.0);
        assert_eq!(p.distance_at(60_000_000), 108.0);
    }

    #[test]
    fn should_report_no_speed_once_the_view_stops_carrying_the_rider() {
        let p = Progress {
            distance_m: 100.0,
            speed_ms: 8.0,
            at_us: Some(1_000_000),
            coast: false,
        };
        assert_eq!(p.speed_at(1_500_000), 8.0);
        // At the very moment the distance stops moving, so does the lean.
        assert_eq!(p.speed_at(2_000_000), 0.0);
        assert_eq!(Progress::default().speed_at(5_000_000), 0.0);
        let bad = Progress {
            speed_ms: f32::NAN,
            ..p
        };
        assert_eq!(bad.speed_at(1_200_000), 0.0);
        let back = Progress {
            speed_ms: -3.0,
            ..p
        };
        assert_eq!(back.speed_at(1_200_000), 0.0);
    }

    #[test]
    fn should_never_run_backwards() {
        // A clock read before the report, or a negative speed from a bad
        // reading, must not pull the camera back up the road.
        let p = Progress {
            distance_m: 100.0,
            speed_ms: -5.0,
            at_us: Some(1_000_000),
            coast: false,
        };
        assert_eq!(p.distance_at(500_000), 100.0);
        assert_eq!(p.distance_at(1_500_000), 100.0);
    }

    /// Frames of a ride at `fps`, from `from_us` for `secs`: the distance the
    /// view draws at each, as the ride reports `reports` (time, distance,
    /// speed) and the view carries the rider on between them.
    fn drawn(reports: &[(i64, f32, f32)], from_us: i64, secs: f32, fps: f32) -> Vec<f32> {
        let mut glide = Glide::default();
        let frames = (secs * fps) as i64;
        (0..frames)
            .map(|k| {
                let now = from_us + (k as f32 * 1e6 / fps) as i64;
                let Some(&(at, d, v)) = reports.iter().rev().find(|r| r.0 <= now) else {
                    return glide.follow(0.0, 0.0, now);
                };
                let p = Progress {
                    distance_m: d,
                    speed_ms: v,
                    at_us: Some(at),
                    coast: false,
                };
                glide.follow(p.distance_at(now), p.speed_at(now), now)
            })
            .collect()
    }

    /// Asserts the drawn rider never goes back and never changes speed from
    /// one frame to the next by much more than the ride itself does, which
    /// is `ride_dv` m/s at most — at 60 frames a second.
    fn assert_smooth(d: &[f32], ride_dv: f32) {
        let steps: Vec<f32> = d.windows(2).map(|w| w[1] - w[0]).collect();
        for (k, &step) in steps.iter().enumerate() {
            assert!(step >= 0.0, "frame {k} went back {step} m");
        }
        for (k, w) in steps.windows(2).enumerate() {
            let dv = (w[1] - w[0]).abs() * 60.0;
            assert!(dv <= ride_dv + 2.0, "frame {k} changed speed by {dv} m/s");
        }
    }

    #[test]
    fn should_not_jolt_the_rider_back_when_they_brake_into_a_bend() {
        // 9 m/s, then the ride brakes for a bend: a second later the rider is
        // at 107.5, not the 109 the view carried them to. Taken as it came,
        // that report put the rider 1.5 m back in one frame.
        let reports = [
            (1_000_000, 100.0, 9.0),
            (2_000_000, 107.5, 6.0),
            (3_000_000, 113.5, 6.0),
        ];
        assert_smooth(&drawn(&reports, 1_000_000, 2.5, 60.0), 3.0);
    }

    #[test]
    fn should_not_jump_the_rider_forward_when_they_speed_up() {
        let reports = [
            (1_000_000, 100.0, 5.0),
            (2_000_000, 106.5, 8.0),
            (3_000_000, 114.5, 8.0),
        ];
        assert_smooth(&drawn(&reports, 1_000_000, 2.5, 60.0), 3.0);
    }

    #[test]
    fn should_glide_onto_where_the_ride_says() {
        let reports = [(1_000_000, 100.0, 9.0), (2_000_000, 107.5, 6.0)];
        let mut more: Vec<(i64, f32, f32)> = reports.to_vec();
        for k in 1..5 {
            more.push((2_000_000 + k * 1_000_000, 107.5 + 6.0 * k as f32, 6.0));
        }
        let d = drawn(&more, 1_000_000, 6.0, 60.0);
        // The last frame is at 6.983 s: the ride has it 4.983 s past 107.5.
        let truth = 107.5 + 6.0 * (359.0 / 60.0 - 1.0);
        let last = d[d.len() - 1];
        assert!(
            (last - truth).abs() < 0.05,
            "drawn at {last}, ridden at {truth}"
        );
    }

    #[test]
    fn should_follow_a_steady_ride_exactly() {
        let reports = [(1_000_000, 100.0, 8.0), (2_000_000, 108.0, 8.0)];
        let d = drawn(&reports, 1_000_000, 1.9, 50.0);
        for (k, got) in d.iter().enumerate() {
            let want = 100.0 + 8.0 * k as f32 / 50.0;
            assert!((got - want).abs() < 1e-3, "frame {k}: {got} vs {want}");
        }
    }

    #[test]
    fn should_ease_the_lean_speed_rather_than_step_it() {
        // Braking from 9 to 6 m/s at a report: in a hairpin a step that size
        // tipped the rider several degrees in one frame.
        let mut glide = Glide::default();
        let mut now = 0i64;
        glide.follow(100.0, 9.0, now);
        let mut last = glide.lean_speed();
        assert_eq!(last, 9.0);
        for k in 1..=180 {
            now += 16_667;
            let v = if k < 30 { 9.0 } else { 6.0 };
            glide.follow(100.0 + k as f32 * 0.12, v, now);
            let step = (glide.lean_speed() - last).abs();
            assert!(step < 0.1, "frame {k}: lean speed stepped {step} m/s");
            last = glide.lean_speed();
        }
        assert!((last - 6.0).abs() < 0.2, "still leaning as at {last} m/s");
    }

    #[test]
    fn should_take_a_skip_along_the_route_at_once() {
        let mut glide = Glide::default();
        assert_eq!(glide.follow(100.0, 8.0, 1_000_000), 100.0);
        assert_eq!(glide.follow(2_000.0, 8.0, 1_016_000), 2_000.0);
        // And back down the road too: a skip back is no glide.
        assert_eq!(glide.follow(500.0, 8.0, 1_032_000), 500.0);
    }

    #[test]
    fn should_settle_where_a_paused_ride_stopped() {
        // Paused a metre short of where the view had carried them: the rider
        // stays put rather than backing up.
        let mut glide = Glide::default();
        glide.follow(100.0, 8.0, 0);
        let shown = glide.follow(108.0, 8.0, 1_000_000);
        let held = glide.follow(107.0, 0.0, 3_000_000);
        assert_eq!(held, shown);
        assert_eq!(glide.follow(107.0, f32::NAN, 4_000_000), shown);
    }

    // ── Status and credit ─────────────────────────────────────────────────────

    #[test]
    fn should_pack_seven_floats_per_vertex() {
        let mesh = Mesh {
            vertices: vec![course3d::Vertex {
                pos: [1.0, 2.0, 3.0],
                colour: [0.1, 0.2, 0.3, 7.0],
            }],
            indices: vec![0, 0, 0],
        };
        let bytes = vertex_bytes(&mesh);
        assert_eq!(bytes.len(), std::mem::size_of::<course3d::Vertex>());
        assert_eq!(&bytes[12..16], &0.1f32.to_ne_bytes());
        assert_eq!(
            &bytes[24..28],
            &7.0f32.to_ne_bytes(),
            "the material last, at the offset GL reads"
        );
        assert_eq!(index_bytes(&mesh).len(), 12);
    }
}

/// Render the 3D view with no window, to a PNG — the only way to see what GL
/// draws without a rider at the screen (the GSK screenshot harness cannot
/// render a GLArea). Uses EGL's surfaceless platform and an offscreen
/// framebuffer, then the widget's own `GlState` for every draw call, so what
/// it shows is what the widget shows.
///
/// ```text
/// CYCLE_3D_GPX=route.gpx CYCLE_3D_OUT=/tmp/view.png [CYCLE_3D_AT=<metres>] \
///   [CYCLE_3D_TIME=<seconds>] [CYCLE_3D_SCENERY=<key>] cargo test render_course_offscreen -- --ignored --nocapture
/// ```
#[cfg(test)]
mod offscreen {
    use super::*;
    use std::ffi::c_void;

    type Egl = *mut c_void;

    const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
    const EGL_NONE: i32 = 0x3038;
    const EGL_OPENGL_API: u32 = 0x30A2;
    const EGL_OPENGL_ES_API: u32 = 0x30A0;
    const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
    const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
    const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
    const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 1;

    const W: i32 = 960;
    const H: i32 = 540;

    #[test]
    #[ignore]
    fn render_course_offscreen() {
        let gpx = std::env::var("CYCLE_3D_GPX").expect("CYCLE_3D_GPX");
        let out = std::env::var("CYCLE_3D_OUT").expect("CYCLE_3D_OUT");
        let mut at: f32 = std::env::var("CYCLE_3D_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200.0);
        let route = Route::from_gpx_path(std::path::Path::new(&gpx)).expect("a GPX");
        let started = std::time::Instant::now();
        // CYCLE_3D_SCENERY=<key>, as stored: desert, mixed, countryside …
        let scenery = std::env::var("CYCLE_3D_SCENERY")
            .ok()
            .map(|k| biome::Scenery::from_key(&k).expect("a known scenery"))
            .unwrap_or_default();
        let scene = CourseScene::build(&route, scenery).expect("a scene");
        eprintln!("scene built in {:.2} s", started.elapsed().as_secs_f64());
        // CYCLE_3D_ENCOUNTER=<n> CYCLE_3D_ENC_S=<seconds>: ride up to the
        // route's n-th encounter at CYCLE_3D_SPEED and show it that many
        // seconds after it began; the rider is wherever that leaves them.
        let mut live = Vec::new();
        for (k, t) in scene.encounters.iter().enumerate().take(12) {
            eprintln!("encounter {k}: {:?} at {:.0} m", t.kind, t.at_m);
        }
        if let Some(n) = std::env::var("CYCLE_3D_ENCOUNTER")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            let t = scene.encounters[n];
            let after: f32 = std::env::var("CYCLE_3D_ENC_S")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0);
            let v: f32 = std::env::var("CYCLE_3D_SPEED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8.0);
            let mut d = encounters::Director::new(vec![t]);
            let (mut m, mut clock, mut began) = (t.at_m - 400.0, 0.0f32, None);
            while clock < 300.0 {
                live = d.update(&scene.road, m, v, clock);
                if !live.is_empty() && began.is_none() {
                    began = Some(clock);
                }
                if began.is_some_and(|b| clock - b >= after) {
                    break;
                }
                m += v / 30.0;
                clock += 1.0 / 30.0;
            }
            at = m;
            eprintln!("{} animals, rider at {at:.0} m", live.len());
        }
        let speed: f32 = std::env::var("CYCLE_3D_SPEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8.0);
        let rider = course3d::rider_pose(&scene.road, at, speed).expect("a rider");
        let pose = course3d::chase_camera(&scene.road, &rider);
        eprintln!(
            "{} vertices, {} indices, {} props; eye {:?} target {:?}",
            scene.mesh.vertices.len(),
            scene.mesh.indices.len(),
            scene.props.len(),
            pose.eye,
            pose.target
        );

        unsafe {
            let lib = libloading::Library::new("libEGL.so.1").expect("libEGL");
            let get_display: libloading::Symbol<
                unsafe extern "C" fn(u32, *mut c_void, *const isize) -> Egl,
            > = lib
                .get(b"eglGetPlatformDisplay\0")
                .expect("eglGetPlatformDisplay");
            let initialize: libloading::Symbol<
                unsafe extern "C" fn(Egl, *mut i32, *mut i32) -> u32,
            > = lib.get(b"eglInitialize\0").expect("eglInitialize");
            let bind_api: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> =
                lib.get(b"eglBindAPI\0").expect("eglBindAPI");
            let create: libloading::Symbol<unsafe extern "C" fn(Egl, Egl, Egl, *const i32) -> Egl> =
                lib.get(b"eglCreateContext\0").expect("eglCreateContext");
            let make_current: libloading::Symbol<unsafe extern "C" fn(Egl, Egl, Egl, Egl) -> u32> =
                lib.get(b"eglMakeCurrent\0").expect("eglMakeCurrent");

            let dpy = get_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!dpy.is_null(), "no surfaceless display");
            let (mut maj, mut min) = (0, 0);
            assert_eq!(initialize(dpy, &mut maj, &mut min), 1, "eglInitialize");
            // CYCLE_3D_ES=1 asks for OpenGL ES 3, as GTK may hand the widget.
            let es = std::env::var("CYCLE_3D_ES").is_ok_and(|v| v == "1");
            assert_eq!(
                bind_api(if es {
                    EGL_OPENGL_ES_API
                } else {
                    EGL_OPENGL_API
                }),
                1
            );
            // Surfaceless offers no configs; EGL_KHR_no_config_context lets a
            // context be made without one, and an FBO needs nothing more.
            let config: Egl = std::ptr::null_mut();
            let ctx_attrs = if es {
                vec![EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE]
            } else {
                vec![
                    EGL_CONTEXT_MAJOR_VERSION,
                    3,
                    EGL_CONTEXT_MINOR_VERSION,
                    3,
                    EGL_CONTEXT_OPENGL_PROFILE_MASK,
                    EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                    EGL_NONE,
                ]
            };
            let ctx = create(dpy, config, std::ptr::null_mut(), ctx_attrs.as_ptr());
            assert!(!ctx.is_null(), "no GL 3.3 core context");
            assert_eq!(
                make_current(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx),
                1
            );

            let get_proc = egl_get_proc_address().expect("eglGetProcAddress");
            let gl = glow::Context::from_loader_function(|name| {
                let c = CString::new(name).expect("a name");
                get_proc(c.as_ptr())
            });
            let fbo = gl.create_framebuffer().expect("fbo");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            let colour = gl.create_renderbuffer().expect("rb");
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(colour));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, W, H);
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::RENDERBUFFER,
                Some(colour),
            );
            let depth = gl.create_renderbuffer().expect("rb");
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::DEPTH_COMPONENT24, W, H);
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::RENDERBUFFER,
                Some(depth),
            );
            assert_eq!(
                gl.check_framebuffer_status(glow::FRAMEBUFFER),
                glow::FRAMEBUFFER_COMPLETE
            );

            let mut state = GlState::new(gl, es).expect("shaders compile");
            // CYCLE_3D_ANIMALS=0: as with the rider's animals switch off.
            let animals = std::env::var("CYCLE_3D_ANIMALS").map_or(true, |v| v != "0");
            if !animals {
                live.clear();
            }
            state.upload(Some(&scene), animals);
            // CYCLE_3D_GTK_STATE=1 leaves the context as GTK's renderer
            // leaves it — depth cleared to 0, depth test on — which once
            // showed the rider nothing but sky.
            if std::env::var("CYCLE_3D_GTK_STATE").is_ok_and(|v| v == "1") {
                state.gl.clear_depth_f32(0.0);
                state.gl.enable(glow::DEPTH_TEST);
                state.gl.depth_func(glow::GREATER);
            }
            // CYCLE_3D_TIME=<seconds> sets where the animals are in their motion.
            let time_s: f32 = std::env::var("CYCLE_3D_TIME")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            // CYCLE_3D_GHOST_GAP=<metres>: the ghost that far ahead of the
            // rider, or behind for a negative gap.
            let ghost_gap: Option<f32> = std::env::var("CYCLE_3D_GHOST_GAP")
                .ok()
                .and_then(|v| v.parse().ok());
            let ghost_at = |m: f32| {
                ghost_gap.and_then(|gap| {
                    course3d::rider_pose_in_lane(
                        &scene.run_out,
                        m + gap,
                        speed,
                        course3d::GHOST_OFFSET_M,
                    )
                })
            };
            // CYCLE_3D_FRAMES=<n>: ride on at CYCLE_3D_SPEED for n frames at
            // 30 a second, writing each to CYCLE_3D_OUT with {} as its number.
            let frames: usize = std::env::var("CYCLE_3D_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            // CYCLE_3D_BENCH=1: time the frames instead of saving them.
            let bench = std::env::var("CYCLE_3D_BENCH").is_ok_and(|v| v == "1");
            let clock = std::time::Instant::now();
            for f in 0..frames {
                let m = at + speed * f as f32 / 30.0;
                let rider = course3d::rider_pose(&scene.road, m, speed).expect("a rider");
                let pose = course3d::chase_camera(&scene.road, &rider);
                let t = time_s + f as f32 / 30.0;
                state.draw(
                    W,
                    H,
                    Some((pose, rider)),
                    ghost_at(m),
                    scene.sky_at(m),
                    t,
                    &live,
                );
                if bench {
                    state.gl.finish();
                } else {
                    save(&state.gl, &out.replace("{}", &format!("{f:03}")));
                }
            }
            if bench {
                let ms = clock.elapsed().as_secs_f64() * 1000.0 / frames.max(1) as f64;
                eprintln!("{frames} frames at {W}x{H}: {ms:.2} ms a frame");
            }
            if frames > 0 {
                return;
            }
            state.draw(
                W,
                H,
                Some((pose, rider)),
                ghost_at(at),
                scene.sky_at(at),
                time_s,
                &live,
            );
            // CYCLE_3D_HUD=1: the fullscreen HUD over the road, through the
            // same texture and pass the widget uses.
            if std::env::var("CYCLE_3D_HUD").is_ok_and(|v| v == "1") {
                let readings = HudReadings {
                    route_name: route.name.clone(),
                    power_watts: Some(180),
                    ftp_watts: 211,
                    speed_kmh: 27.4,
                    to_go_m: (route.total_distance_m - at).max(0.0),
                    heart_rate_bpm: Some(142),
                    cadence_rpm: None,
                    gradient_pct: route.gradient_at(at) * 100.0,
                    elapsed_secs: 872,
                    climbed_m: 62.0,
                    progress: at / route.total_distance_m.max(1.0),
                    ghost_gap_secs: None,
                    ghost_progress: None,
                };
                let mut image = RideHud::new()
                    .render(W, H, &readings, &ride_hud::profile(&route))
                    .expect("a HUD");
                state.upload_hud(&mut image);
                state.draw_hud(W, H);
            }
            let err = state.gl.get_error();
            eprintln!("glGetError after draw: 0x{err:X}");
            save(&state.gl, &out);
        }
    }

    /// What is in the framebuffer, to a PNG at `out`.
    unsafe fn save(gl: &glow::Context, out: &str) {
        {
            let mut pixels = vec![0u8; (W * H * 4) as usize];
            gl.read_pixels(
                0,
                0,
                W,
                H,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            // GL's rows run bottom-up; images run top-down.
            let row = (W * 4) as usize;
            let flipped: Vec<u8> = pixels.chunks(row).rev().flatten().copied().collect();
            let texture = gtk::gdk::MemoryTexture::new(
                W,
                H,
                gtk::gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(flipped),
                row,
            );
            texture.save_to_png(out).expect("saved");
            eprintln!("wrote {out}");
        }
    }
}
