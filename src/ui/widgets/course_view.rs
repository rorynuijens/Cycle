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
use crate::data::palette;
use crate::data::props::{self, Shape};
use crate::data::rider;
use crate::data::route::Route;

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
uniform float u_orbit;
uniform float u_head;
uniform vec2 u_hub;
out vec3 v_world;
out vec3 v_rel;
flat out vec4 v_colour;
flat out vec3 v_anchor;
void main() {
    vec3 base = i_pos;
    vec3 p = a_pos;
    float yaw = i_size.y;
    float phase = i_motion.y;
    if (i_motion.x > 0.5 && i_motion.x < 1.5) {
        yaw += 0.35 * sin(u_time * 0.25 + phase);
        if (p.x > u_head) {
            float dip = 0.5 + 0.5 * sin(u_time * 0.8 + phase * 3.0);
            p.y -= 0.3 * dip;
            p.x -= 0.06 * dip;
        }
    } else if (i_motion.x > 3.5) {
        // Sails: everything in front of the hub turns about it.
        if (p.x > u_hub.x) {
            float a = u_time * 0.9 + phase;
            float c0 = cos(a);
            float s0 = sin(a);
            vec2 q = vec2(p.z, p.y - u_hub.y);
            p.z = q.x * c0 - q.y * s0;
            p.y = u_hub.y + q.x * s0 + q.y * c0;
        }
    } else if (i_motion.x > 2.5) {
        // Afloat: rising and falling, turning a little on the swell.
        base.y += 0.12 * sin(u_time * 1.3 + phase);
        yaw += 0.08 * sin(u_time * 0.5 + phase * 2.0);
    } else if (i_motion.x > 1.5) {
        float a = u_time * 0.35 + phase;
        base += vec3(cos(a) * u_orbit, 1.5 * sin(u_time * 0.6 + phase), sin(a) * u_orbit);
        yaw = a + 1.5707963;
        p.y += sin(u_time * 9.0 + phase * 5.0) * 0.6 * abs(p.z);
    }
    float c = cos(yaw);
    float s = sin(yaw);
    p *= i_size.x;
    vec3 world = base + vec3(c * p.x - s * p.z, p.y, s * p.x + c * p.z);
    v_world = world;
    v_rel = world - u_eye;
    v_colour = a_colour.r < 0.0 ? vec4(i_colour, a_colour.a) : a_colour;
    v_anchor = world;
    gl_Position = u_mvp * vec4(world, 1.0);
}
"#;

/// Toon shading: three bands of sunlight, a touch of sky on faces that look
/// up, a rim of haze light on edges turned away, and squared-exponential
/// haze. The horizon's mountains take a fixed haze of their own, thicker at
/// their feet; clouds take none. Writes the colour and the face's normal;
/// the outline pass reads both.
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
layout(location = 0) out vec4 frag;
layout(location = 1) out vec4 frag_normal;

float hash(vec3 p) {
    return fract(sin(dot(p, vec3(127.1, 311.7, 74.7))) * 43758.5453);
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
    if (material == 1.0) {
        // A hand-made look: each facet of the land a few per cent lighter or
        // darker, seeded by where it is.
        c *= 0.96 + 0.08 * hash(floor(v_anchor));
    } else if (material == 4.0) {
        // Water lies flat, with lighter stripes drifting along it.
        n = vec3(0.0, 1.0, 0.0);
        float w = sin(v_world.x * 0.21 + v_world.z * 0.13 + u_time * 1.6)
            * sin(v_world.z * 0.17 - v_world.x * 0.07 - u_time * 1.1);
        c = mix(c, u_foam, 0.35 * step(0.55, w));
    }
    vec3 sun = normalize(u_sun);
    float ndl = dot(n, sun);
    float band = ndl > 0.5 ? 1.0 : (ndl > 0.1 ? 0.9 : 0.8);
    vec3 ambient = mix(vec3(1.0), u_zenith * 1.6, 0.12 * (0.5 + 0.5 * n.y));
    float rim = pow(1.0 - clamp(dot(n, normalize(-v_rel)), 0.0, 1.0), 4.0);
    vec3 lit = c * (0.9 * band + 0.12) * ambient + u_horizon * rim * 0.1;
    if (material == 5.0) {
        // Far mountains: pale with distance, palest at their feet.
        float up = clamp((v_rel.y + 120.0) / 1100.0, 0.0, 1.0);
        frag = vec4(mix(lit, u_horizon, mix(0.72, 0.32, up)), 1.0);
        frag_normal = vec4(0.5, 1.0, 0.5, 1.0);
        return;
    }
    if (material == 6.0) {
        // Clouds: white, a little shade underneath, no haze.
        frag = vec4(c * (0.9 + 0.1 * clamp(n.y, 0.0, 1.0)) - 0.06 * clamp(-n.y, 0.0, 1.0), 1.0);
        frag_normal = vec4(0.5, 1.0, 0.5, 1.0);
        return;
    }
    float k = max(length(v_rel) - u_fog_start, 0.0) * u_fog_density;
    frag = vec4(mix(lit, u_horizon, 1.0 - exp(-k * k)), 1.0);
    frag_normal = vec4(n * 0.5 + 0.5, 1.0);
}
"#;

/// One triangle over the whole view, for the sky and the outline pass.
const FULLSCREEN_VERTEX_SHADER: &str = r#"
out vec2 v_uv;
void main() {
    vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
    v_uv = p * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

/// The sky: shaded from haze at the horizon to blue overhead. Drawn first,
/// without depth, under everything.
const SKY_FRAGMENT_SHADER: &str = r#"
in vec2 v_uv;
uniform vec3 u_horizon;
uniform vec3 u_zenith;
layout(location = 0) out vec4 frag;
layout(location = 1) out vec4 frag_normal;
void main() {
    frag = vec4(mix(u_horizon, u_zenith, smoothstep(0.45, 1.0, v_uv.y)), 1.0);
    frag_normal = vec4(0.5, 1.0, 0.5, 1.0);
}
"#;

/// Outlines: dark lines where the depth folds — an object's silhouette
/// against what is behind it — or where the faces either side turn sharply,
/// as at a building's corner. Gentle folds in the land are left unlined.
/// Lines fade into the haze with what they outline.
const OUTLINE_FRAGMENT_SHADER: &str = r#"
precision highp sampler2D;
in vec2 v_uv;
uniform sampler2D u_colour;
uniform sampler2D u_normal;
uniform sampler2D u_depth;
uniform vec2 u_texel;
uniform float u_near;
uniform float u_far;
uniform vec3 u_outline;
uniform float u_fog_start;
uniform float u_fog_density;
out vec4 frag;

float linear(float z) {
    float ndc = z * 2.0 - 1.0;
    return 2.0 * u_near * u_far / (u_far + u_near - ndc * (u_far - u_near));
}

void main() {
    vec2 dx = vec2(u_texel.x, 0.0);
    vec2 dy = vec2(0.0, u_texel.y);
    float dc = linear(texture(u_depth, v_uv).r);
    float dl = linear(texture(u_depth, v_uv - dx).r);
    float dr = linear(texture(u_depth, v_uv + dx).r);
    float dd = linear(texture(u_depth, v_uv - dy).r);
    float du = linear(texture(u_depth, v_uv + dy).r);
    // On a flat face the inverse depth changes evenly across the screen, so
    // its second difference is zero; at a silhouette or a crease it is not.
    float fold = (abs(1.0 / dl + 1.0 / dr - 2.0 / dc) + abs(1.0 / dd + 1.0 / du - 2.0 / dc)) * dc;
    float edge = smoothstep(0.03, 0.08, fold);
    vec3 nc = texture(u_normal, v_uv).xyz * 2.0 - 1.0;
    float turn = min(
        min(dot(nc, texture(u_normal, v_uv - dx).xyz * 2.0 - 1.0),
            dot(nc, texture(u_normal, v_uv + dx).xyz * 2.0 - 1.0)),
        min(dot(nc, texture(u_normal, v_uv - dy).xyz * 2.0 - 1.0),
            dot(nc, texture(u_normal, v_uv + dy).xyz * 2.0 - 1.0)));
    edge = max(edge, smoothstep(0.75, 0.5, turn));
    float nearest = min(dc, min(min(dl, dr), min(dd, du)));
    float k = max(nearest - u_fog_start, 0.0) * u_fog_density;
    float clear = exp(-k * k);
    vec3 c = texture(u_colour, v_uv).rgb;
    frag = vec4(mix(c, u_outline, 0.85 * edge * clear), 1.0);
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
}

impl Progress {
    /// Where the rider is at `now_us`: the last report carried on at its speed
    /// for at most [`MAX_EXTRAPOLATE_S`].
    fn distance_at(&self, now_us: i64) -> f32 {
        let Some(at) = self.at_us else {
            return self.distance_m;
        };
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
        if dt < MAX_EXTRAPOLATE_S && self.speed_ms.is_finite() {
            self.speed_ms.max(0.0)
        } else {
            0.0
        }
    }
}

/// Everything living in the GL context. Created on realize, dropped on
/// unrealize — the context and everything in it go together.
struct GlState {
    gl: glow::Context,
    program: glow::Program,
    sky_program: glow::Program,
    outline_program: glow::Program,
    prop_program: glow::Program,
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
    /// One per prop shape: its mesh, and every prop of that shape.
    props: Vec<PropShape>,
    /// The scene is drawn here first — colour, normals and depth — and the
    /// outline pass draws it into GTK's framebuffer.
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
    normal: glow::Texture,
    depth: glow::Texture,
    size: (i32, i32),
}

/// One prop shape on the GPU: its mesh, and the props drawn with it.
struct PropShape {
    shape: Shape,
    mesh: MeshGl,
    instances: glow::Buffer,
    chunks: Vec<props::Chunk>,
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
        }
    }

    /// Set every one for a frame. The model starts as the identity.
    unsafe fn set(&self, gl: &glow::Context, mvp: &Mat4, eye: &[f32; 3], sky: &Sky, time_s: f32) {
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
    }
}

/// Every uniform the programs read.
struct Uniforms {
    scene: SceneUniforms,
    props: SceneUniforms,
    sky_horizon: Uniform,
    sky_zenith: Uniform,
    o_colour: Uniform,
    o_normal: Uniform,
    o_depth: Uniform,
    o_texel: Uniform,
    o_near: Uniform,
    o_far: Uniform,
    o_outline: Uniform,
    o_fog_start: Uniform,
    o_fog_density: Uniform,
}

struct Inner {
    root: gtk::Overlay,
    area: gtk::GLArea,
    /// "Building the 3D view…", over the top left of the view.
    status: gtk::Label,
    route: RefCell<Option<Route>>,
    /// What the world around the route is made of.
    scenery: Cell<biome::Scenery>,
    scene: RefCell<Option<CourseScene>>,
    /// The scene changed and has not been uploaded yet.
    needs_upload: Cell<bool>,
    gl: RefCell<Option<GlState>>,
    progress: Cell<Progress>,
    /// The distance last drawn, so an unmoving camera is not redrawn.
    drawn_m: Cell<Option<f32>>,
    /// Bumped for every new route, so a build that finishes after the next
    /// route has been set is thrown away rather than shown.
    generation: Cell<u64>,
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

        let inner = Rc::new(Inner {
            root: root.clone(),
            status,
            area: area.clone(),
            route: RefCell::new(None),
            scenery: Cell::new(biome::Scenery::default()),
            scene: RefCell::new(None),
            needs_upload: Cell::new(false),
            gl: RefCell::new(None),
            progress: Cell::new(Progress::default()),
            drawn_m: Cell::new(None),
            generation: Cell::new(0),
        });

        // Handlers hold the inner state weakly: the area owns these closures,
        // and a strong capture would keep it alive forever (CLAUDE.md §2.4).
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

    /// Show `route` in `scenery`, with the rider at its start. The world is
    /// built off the main thread; the view says so until it is up.
    pub fn set_route(&self, route: &Route, scenery: biome::Scenery) {
        let inner = &self.inner;
        inner.generation.set(inner.generation.get() + 1);
        *inner.scene.borrow_mut() = None;
        inner.needs_upload.set(true);
        inner.progress.set(Progress::default());
        inner.drawn_m.set(None);
        *inner.route.borrow_mut() = Some(route.clone());
        inner.scenery.set(scenery);
        Inner::rebuild(inner);
    }

    /// Where the ride is: `distance_m` along the route, moving at `speed_ms`.
    /// Called once a tick by the ride loop.
    pub fn set_progress(&self, distance_m: f32, speed_ms: f32) {
        self.inner.progress.set(Progress {
            distance_m,
            speed_ms,
            at_us: Some(glib::monotonic_time()),
        });
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
        let scenery = this.scenery.get();
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
                    *inner.scene.borrow_mut() = scene;
                    inner.needs_upload.set(true);
                    inner.area.queue_render();
                }
                Err(_) => tracing::error!("Building the 3D course panicked"),
            }
            inner.update_status();
        });
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
            state.upload(scene.as_ref());
        }

        let scale = area.scale_factor();
        let (w, h) = (area.width() * scale, area.height() * scale);
        let now = glib::monotonic_time();
        let progress = self.progress.get();
        let distance = progress.distance_at(now);
        let speed = progress.speed_at(now);
        let (pose, sky) = match self.scene.borrow().as_ref() {
            Some(s) => (
                course3d::rider_pose(&s.road, distance, speed)
                    .map(|r| (course3d::chase_camera(&s.road, &r), r)),
                s.sky_at(distance),
            ),
            None => (None, Sky::default()),
        };
        // Seconds, wrapped every hour so the animals' motion keeps its
        // precision.
        let time_s = (now / 1_000 % 3_600_000) as f32 / 1_000.0;

        state.draw(w, h, pose, sky, time_s);
        self.drawn_m.set(pose.map(|_| distance));
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
            let outline_program = link(
                &gl,
                header,
                FULLSCREEN_VERTEX_SHADER,
                OUTLINE_FRAGMENT_SHADER,
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
                o_colour: at(outline_program, "u_colour"),
                o_normal: at(outline_program, "u_normal"),
                o_depth: at(outline_program, "u_depth"),
                o_texel: at(outline_program, "u_texel"),
                o_near: at(outline_program, "u_near"),
                o_far: at(outline_program, "u_far"),
                o_outline: at(outline_program, "u_outline"),
                o_fog_start: at(outline_program, "u_fog_start"),
                o_fog_density: at(outline_program, "u_fog_density"),
            };
            Ok(Self {
                gl,
                program,
                sky_program,
                outline_program,
                prop_program,
                empty_vao,
                world,
                horizons,
                clouds,
                rider_body,
                rider_leg,
                props,
                target: None,
                u,
                reported: Cell::new(false),
            })
        }
    }

    /// Replace what is on the GPU with `scene`, or with nothing.
    fn upload(&mut self, scene: Option<&CourseScene>) {
        let batches = scene.map(|s| props::batches(&s.props)).unwrap_or_default();
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

    fn draw(
        &mut self,
        width: i32,
        height: i32,
        pose: Option<(CameraPose, RiderPose)>,
        sky: Sky,
        time_s: f32,
    ) {
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
            Ok(t) => (t.fbo, t.colour, t.normal, t.depth),
            Err(e) => {
                tracing::error!("3D view could not make its framebuffer: {e}");
                return;
            }
        };
        let (fbo, colour_tex, normal_tex, depth_tex) = target;
        let gl = &self.gl;
        let u = &self.u;
        let outline = palette::BASE.outline;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.draw_buffers(&[glow::COLOR_ATTACHMENT0, glow::COLOR_ATTACHMENT1]);
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
                let body = course3d::rider_model(&rider_pose);
                gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &body);
                self.rider_body.draw(gl);
                for side in [-1.0f32, 1.0] {
                    let (swing, lift) = rider::leg_pose(rider_pose.crank, side);
                    let hip = course3d::translation([
                        rider::HIP[0],
                        rider::HIP[1] + lift,
                        rider::HIP[2] + side * rider::LEG_Z,
                    ]);
                    let leg = course3d::mul4(&body, &course3d::mul4(&hip, &course3d::pitch(swing)));
                    gl.uniform_matrix_4_f32_slice(u.scene.model.as_ref(), false, &leg);
                    self.rider_leg.draw(gl);
                }

                gl.use_program(Some(self.prop_program));
                u.props.set(gl, &mvp, &camera.eye, &sky, time_s);
                for shape in &self.props {
                    shape.draw(gl, camera.eye);
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

            // Into GTK's framebuffer, with the outlines.
            gl.bind_framebuffer(glow::FRAMEBUFFER, screen);
            gl.viewport(0, 0, width, height);
            gl.use_program(Some(self.outline_program));
            for (unit, tex, uniform) in [
                (0, colour_tex, &u.o_colour),
                (1, normal_tex, &u.o_normal),
                (2, depth_tex, &u.o_depth),
            ] {
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.uniform_1_i32(uniform.as_ref(), unit as i32);
            }
            gl.uniform_2_f32(
                u.o_texel.as_ref(),
                1.0 / width.max(1) as f32,
                1.0 / height.max(1) as f32,
            );
            gl.uniform_1_f32(u.o_near.as_ref(), NEAR_M);
            gl.uniform_1_f32(u.o_far.as_ref(), FAR_M);
            gl.uniform_3_f32_slice(u.o_outline.as_ref(), &outline);
            gl.uniform_1_f32(u.o_fog_start.as_ref(), biome::FOG_START_M);
            gl.uniform_1_f32(u.o_fog_density.as_ref(), sky.fog_density);
            gl.bind_vertex_array(Some(self.empty_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.use_program(None);
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
        for mesh in [&self.world, &self.clouds, &self.rider_body, &self.rider_leg] {
            mesh.destroy(&self.gl);
        }
        for horizon in &self.horizons {
            horizon.destroy(&self.gl);
        }
        for shape in &self.props {
            shape.mesh.destroy(&self.gl);
            unsafe { self.gl.delete_buffer(shape.instances) };
        }
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.sky_program);
            self.gl.delete_program(self.outline_program);
            self.gl.delete_program(self.prop_program);
            self.gl.delete_vertex_array(self.empty_vao);
        }
    }
}

impl PropShape {
    /// The shape's mesh on the GPU, with an empty buffer for its props.
    unsafe fn new(gl: &glow::Context, shape: Shape) -> Result<Self, String> {
        let mesh = MeshGl::new(gl, &shape.mesh())?;
        let instances = gl.create_buffer()?;
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
        })
    }

    /// Draw the props of every chunk within reach of the eye. Each chunk's
    /// run is reached by pointing the per-prop attributes at its start: GL
    /// ES 3.0 has no base instance.
    unsafe fn draw(&self, gl: &glow::Context, eye: [f32; 3]) {
        if self.chunks.is_empty() || self.mesh.index_count == 0 {
            return;
        }
        let stride = (props::INSTANCE_FLOATS * 4) as i32;
        gl.bind_vertex_array(Some(self.mesh.vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.instances));
        for chunk in &self.chunks {
            if !props::in_reach(chunk, eye[0], eye[2], PROP_DRAW_M) {
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
    /// A framebuffer with a colour and a normal texture and a depth texture,
    /// all `size`, every one readable by the outline pass.
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
        let normal = texture(glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE)?;
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
            (glow::COLOR_ATTACHMENT1, normal),
            (glow::DEPTH_ATTACHMENT, depth),
        ] {
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, attachment, glow::TEXTURE_2D, Some(t), 0);
        }
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        let target = Self {
            fbo,
            colour,
            normal,
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
            gl.delete_texture(self.normal);
            gl.delete_texture(self.depth);
        }
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
        };
        assert_eq!(p.distance_at(500_000), 100.0);
        assert_eq!(p.distance_at(1_500_000), 100.0);
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
        let at: f32 = std::env::var("CYCLE_3D_AT")
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
            state.upload(Some(&scene));
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
            state.draw(W, H, Some((pose, rider)), scene.sky_at(at), time_s);
            let err = state.gl.get_error();
            eprintln!("glGetError after draw: 0x{err:X}");

            let mut pixels = vec![0u8; (W * H * 4) as usize];
            state.gl.read_pixels(
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
            texture.save_to_png(&out).expect("saved");
            eprintln!("wrote {out}");
        }
    }
}
