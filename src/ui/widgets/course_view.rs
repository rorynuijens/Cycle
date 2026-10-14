//! The road from the saddle: a 3D view of the route, drawn with OpenGL.
//!
//! Everything it draws comes from [`crate::data::course3d`]; this module only
//! uploads that geometry and moves the camera. The ride loop reports distance
//! once a second, and a camera that jumped once a second would be unwatchable,
//! so between reports the view carries the rider on at their last speed —
//! for at most a second, so a paused ride stops instead of driving on.
//!
//! The land comes in after the road. A route first shows on flat ground; the
//! first time the view is shown for it, the elevation tiles are fetched (or
//! read from the cache) on the tokio runtime, decoded off the main thread, and
//! the scene is rebuilt with real terrain. Riding never waits on the network.
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

use crate::data::course3d::{self, CourseScene, Mesh};
use crate::data::osm::{self, Features};
use crate::data::route::Route;
use crate::data::terrain::{self, Dem, TileId};
use std::sync::Arc;

/// Vertical field of view, in degrees. Roughly what a rider takes in looking
/// down the road, and wide enough that a bend does not leave the frame.
const FOV_DEG: f32 = 60.0;
const NEAR_M: f32 = 0.5;
const FAR_M: f32 = 3000.0;

/// Haze: geometry fades into the sky between these distances. It is what
/// hides the edge of the ground, so it must be complete before the ground
/// runs out — the eye is on the road, and the ground reaches
/// `course3d::GROUND_RADIUS_M` from it in every direction.
const FOG_START_M: f32 = 300.0;
const FOG_END_M: f32 = 1_150.0;
const _: () = assert!(FOG_END_M < course3d::GROUND_RADIUS_M);

/// Haze at the horizon, and the sky overhead. Fixed, like the scene — see
/// `course3d`.
const SKY_RGB: [f32; 3] = [0.72, 0.81, 0.90];
const ZENITH_RGB: [f32; 3] = [0.38, 0.56, 0.80];

/// Where the sun is: high in the south-west, so slopes facing it are lit
/// and slopes facing away fall into shade. Not normalised here; the shader does.
const SUN_DIR: [f32; 3] = [-0.45, 0.85, 0.40];

const BUILDING: &str = "Building the 3D view…";

/// Credits for what is on screen. OpenStreetMap's is required by its licence
/// wherever its data is shown.
const MAP_CREDIT: &str = "Map data © OpenStreetMap contributors";
const MAP_SOURCES: &str = "Buildings, land use, water and coastline from OpenStreetMap, \
    available under the Open Database License.";
const ELEVATION_CREDIT: &str = "Elevation: Terrain Tiles · Mapzen, AWS Open Data";
const ELEVATION_SOURCES: &str = "The Terrain Tiles combine open elevation data from \
    3DEP, SRTM, GMTED2010, ETOPO1, EU-DEM (Copernicus), ArcticDEM and national \
    mapping agencies, hosted on the AWS Registry of Open Data.";

/// Longest the view will carry the rider on without a fresh distance, in
/// seconds. A tick is due every second; past that, the ride has paused.
const MAX_EXTRAPOLATE_S: f32 = 1.0;

/// A camera that moved less than this since the last frame is not redrawn.
const REDRAW_EPSILON_M: f32 = 0.001;

const VERTEX_SHADER: &str = r#"
in vec3 a_pos;
in vec4 a_colour;
in vec2 a_uv;
uniform mat4 u_mvp;
uniform vec3 u_eye;
out vec3 v_world;
out vec3 v_rel;
out vec4 v_colour;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    v_world = a_pos;
    // Relative to the eye, the numbers are small. Interpolated across a pixel
    // at thousands of metres from the scene's origin, the absolute position
    // loses the precision the lighting takes its slope from, and walls came
    // out speckled.
    v_rel = a_pos - u_eye;
    v_colour = a_colour;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"
in vec3 v_world;
in vec3 v_rel;
in vec4 v_colour;
in vec2 v_uv;
uniform vec3 u_sky;
uniform vec3 u_sun;
uniform float u_fog_start;
uniform float u_fog_end;
out vec4 frag;

float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
    vec3 c = v_colour.rgb;
    // The material is the whole part of the alpha; walls keep a fraction too.
    float material = floor(v_colour.a + 0.0005);
    // The face normal comes from how the position changes across the pixel,
    // so no normals are stored.
    vec3 n = normalize(cross(dFdx(v_rel), dFdy(v_rel)));
    // Face the viewer. Flipping to face *up* instead decides a wall's side by
    // the sign of a vertical component that is noise, and the wall comes out
    // speckled light and dark pixel by pixel.
    if (dot(n, v_rel) > 0.0) n = -n;
    if (material == 1.0) {
        // Ground: two scales of mottling, so the grass slides past and the
        // eye can read speed from it.
        float fine = hash(floor(v_world.xz / 2.0));
        float coarse = hash(floor(v_world.xz / 14.0));
        c *= 0.84 + 0.16 * fine + 0.12 * (coarse - 0.5);
    } else if (material == 2.0) {
        // Asphalt: a fine grain, faded out where a grain is under a pixel so
        // the distant road does not shimmer.
        float grain = hash(floor(v_world.xz / 0.12));
        float fade = 1.0 - smoothstep(0.04, 0.12, fwidth(v_world.x) + fwidth(v_world.z));
        c *= 1.0 + (grain - 0.5) * 0.07 * fade;
    } else if (material == 3.0) {
        // Walls: a window in the middle half of every bay, every storey,
        // laid out by the wall's own coordinates — bays along, metres up.
        float storey = floor(v_uv.y / 3.0);
        float bay = floor(v_uv.x);
        float fx = v_uv.x - bay;
        float fy = v_uv.y - storey * 3.0;
        // Fade the grid out once a window is only a few pixels across.
        float px = max(fwidth(v_uv.x) * 2.8, fwidth(v_uv.y));
        float fade = 1.0 - smoothstep(0.25, 0.6, px);
        if (v_uv.x >= 0.0 && storey >= 0.0 && fx > 0.25 && fx < 0.75 && fy > 0.9 && fy < 2.2) {
            float seed = fract(v_colour.a) * 97.0;
            float lit = hash(vec2(bay + seed, storey - seed));
            vec3 glass = mix(vec3(0.15, 0.19, 0.24), vec3(0.34, 0.38, 0.42), lit);
            c = mix(c, glass, fade);
        }
    }
    // Light by how the face turns to the sun.
    c *= 0.62 + 0.45 * max(dot(n, normalize(u_sun)), 0.0);
    float d = length(v_rel);
    float f = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    frag = vec4(mix(c, u_sky, f), 1.0);
}
"#;

/// The sky: one triangle over the whole view, shaded from haze at the
/// horizon to blue overhead. Drawn first, without depth, under everything.
const SKY_VERTEX_SHADER: &str = r#"
out float v_up;
void main() {
    vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
    v_up = p.y * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

const SKY_FRAGMENT_SHADER: &str = r#"
in float v_up;
uniform vec3 u_horizon;
uniform vec3 u_zenith;
out vec4 frag;
void main() {
    frag = vec4(mix(u_horizon, u_zenith, smoothstep(0.45, 1.0, v_up)), 1.0);
}
"#;

/// How one of the view's two downloads — terrain, map data — stands for the
/// route on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    /// Not asked for yet: the view has not been shown for this route.
    Wanted,
    Loading,
    Ready,
    /// Tried and failed; showing the view again tries again.
    Failed,
    /// Will never load for this route — it is too large to fetch.
    Unavailable,
}

impl Load {
    fn should_start(self) -> bool {
        matches!(self, Self::Wanted | Self::Failed)
    }
}

/// What the status line should say, given where both downloads stand and
/// whether a scene is up yet. `None` is nothing to say.
fn status_text(has_scene: bool, terrain: Load, map: Load) -> Option<&'static str> {
    if !has_scene {
        return Some(BUILDING);
    }
    Some(match (terrain, map) {
        (Load::Loading, _) => "Loading terrain…",
        (_, Load::Loading) => "Loading map data — the first time for a route takes up to a minute",
        (Load::Failed, _) => "No terrain — check the connection. Flat ground for now",
        (_, Load::Failed) => "No map data — check the connection. Open country for now",
        (Load::Unavailable, _) => "This route is too large for terrain",
        (_, Load::Unavailable) => "This route is too large for map data",
        _ => return None,
    })
}

/// The credit line as markup: OpenStreetMap's credit links to its copyright
/// page, as the OSMF attribution guidelines ask — a click from the 3D view
/// reaches the licence. The rest is escaped plain text.
fn credit_markup(text: &str) -> String {
    let escaped = glib::markup_escape_text(text).to_string();
    escaped.replace(
        "© OpenStreetMap contributors",
        "© <a href=\"https://www.openstreetmap.org/copyright\">OpenStreetMap contributors</a>",
    )
}

/// The credit line and its tooltip for a scene built with terrain
/// (`elevation`) and map data (`map`).
fn credit_text(elevation: bool, map: bool) -> Option<(String, String)> {
    match (elevation, map) {
        (true, true) => Some((
            format!("{MAP_CREDIT} · {ELEVATION_CREDIT}"),
            format!("{MAP_SOURCES}\n{ELEVATION_SOURCES}"),
        )),
        (false, true) => Some((MAP_CREDIT.into(), MAP_SOURCES.into())),
        (true, false) => Some((ELEVATION_CREDIT.into(), ELEVATION_SOURCES.into())),
        (false, false) => None,
    }
}

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
}

/// Everything living in the GL context. Created on realize, dropped on
/// unrealize — the context and everything in it go together.
struct GlState {
    gl: glow::Context,
    program: glow::Program,
    sky_program: glow::Program,
    /// Core profiles draw nothing without a vertex array bound, even for a
    /// triangle made up in the shader.
    sky_vao: glow::VertexArray,
    u_horizon: Option<glow::UniformLocation>,
    u_zenith: Option<glow::UniformLocation>,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    index_count: i32,
    u_mvp: Option<glow::UniformLocation>,
    u_eye: Option<glow::UniformLocation>,
    u_sky: Option<glow::UniformLocation>,
    u_sun: Option<glow::UniformLocation>,
    u_fog_start: Option<glow::UniformLocation>,
    u_fog_end: Option<glow::UniformLocation>,
    /// Whether the GL state GTK handed over has been logged — once per
    /// context, for the day a rider's screen shows only sky.
    reported: Cell<bool>,
}

struct Inner {
    root: gtk::Overlay,
    area: gtk::GLArea,
    /// "Loading terrain…" and the like, over the top left of the view.
    status: gtk::Label,
    /// The elevation data's credit, over the bottom right.
    credit: gtk::Label,
    /// Where downloads run; `None` until the window hands it over, and in the
    /// screenshot harness, which never fetches anything.
    rt: RefCell<Option<tokio::runtime::Handle>>,
    route: RefCell<Option<Route>>,
    terrain: Cell<Load>,
    map: Cell<Load>,
    dem: RefCell<Option<Arc<Dem>>>,
    features: RefCell<Option<Arc<Features>>>,
    /// Numbers each scene build; a build finishing after a newer one started
    /// is dropped, so a slow early build cannot replace a fuller later one.
    build_seq: Cell<u64>,
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
            "The road ahead, seen from the saddle",
        )]);
        let status = gtk::Label::builder()
            .css_classes(["caption", "osd"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .visible(false)
            .build();
        let credit = gtk::Label::builder()
            .label(ELEVATION_CREDIT)
            .use_markup(true)
            .tooltip_text(ELEVATION_SOURCES)
            .css_classes(["caption", "osd"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(6)
            .margin_end(6)
            .visible(false)
            .build();
        let root = gtk::Overlay::builder().child(&area).build();
        root.add_overlay(&status);
        root.add_overlay(&credit);

        let inner = Rc::new(Inner {
            root: root.clone(),
            status,
            credit,
            rt: RefCell::new(None),
            route: RefCell::new(None),
            terrain: Cell::new(Load::Wanted),
            map: Cell::new(Load::Wanted),
            dem: RefCell::new(None),
            features: RefCell::new(None),
            build_seq: Cell::new(0),
            area: area.clone(),
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
        // The land is fetched the first time the view is actually shown for a
        // route — a rider who never opens 3D never downloads anything.
        let weak = Rc::downgrade(&inner);
        root.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                Inner::ensure_world(&inner);
            }
        });
        // Runs only while the area is mapped, so a hidden view costs nothing.
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
            if moved || inner.needs_upload.get() {
                area.queue_render();
            }
            glib::ControlFlow::Continue
        });

        Self { inner }
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.inner.root
    }

    /// Hand over the runtime terrain downloads run on.
    pub fn set_runtime(&self, rt: tokio::runtime::Handle) {
        *self.inner.rt.borrow_mut() = Some(rt);
    }

    /// Show `route`, with the rider at its start.
    ///
    /// The road goes up on flat ground at once; terrain and map data follow
    /// the first time the view is shown, and the scene is rebuilt as each
    /// arrives.
    pub fn set_route(&self, route: &Route) {
        let inner = &self.inner;
        inner.generation.set(inner.generation.get() + 1);
        *inner.scene.borrow_mut() = None;
        inner.needs_upload.set(true);
        inner.progress.set(Progress::default());
        inner.drawn_m.set(None);
        *inner.route.borrow_mut() = Some(route.clone());
        inner.terrain.set(Load::Wanted);
        inner.map.set(Load::Wanted);
        *inner.dem.borrow_mut() = None;
        *inner.features.borrow_mut() = None;
        inner.credit.set_visible(false);
        Inner::rebuild(inner);
        if inner.root.is_mapped() {
            Inner::ensure_world(inner);
        }
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
    /// Start whichever of terrain and map data the current route still needs.
    /// A failed download is tried again each time the view is shown — with
    /// whatever did arrive already in the cache.
    fn ensure_world(this: &Rc<Self>) {
        let Some(rt) = this.rt.borrow().clone() else {
            return;
        };
        let Some(route) = this.route.borrow().clone() else {
            return;
        };
        if route.points.len() < 2 {
            return;
        }
        if this.terrain.get().should_start() {
            Self::start_terrain(this, &rt, &route);
        }
        if this.map.get().should_start() {
            Self::start_map(this, &rt, route);
        }
        this.update_status();
    }

    fn start_terrain(this: &Rc<Self>, rt: &tokio::runtime::Handle, route: &Route) {
        let ids = match terrain::tiles_for_route(route, terrain::CORRIDOR_M) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("No terrain for this route: {e}");
                this.terrain.set(Load::Unavailable);
                return;
            }
        };
        this.terrain.set(Load::Loading);
        let generation = this.generation.get();
        let weak = Rc::downgrade(this);
        crate::ui::spawn_to_main(
            rt,
            terrain::load(terrain::cache_dir(), ids),
            move |loaded| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.generation.get() != generation {
                    return;
                }
                match loaded {
                    Ok(tiles) => Self::decode_terrain(&inner, tiles, generation),
                    Err(e) => {
                        tracing::warn!("Terrain did not load: {e:#}");
                        inner.terrain.set(Load::Failed);
                        inner.update_status();
                    }
                }
            },
        );
    }

    /// Decode the tiles off the main thread, then rebuild on them.
    fn decode_terrain(this: &Rc<Self>, tiles: Vec<(TileId, Vec<u8>)>, generation: u64) {
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            let decoded = gtk::gio::spawn_blocking(move || decode_tiles(tiles)).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.generation.get() != generation {
                return;
            }
            match decoded {
                Ok(Ok(dem)) => {
                    *inner.dem.borrow_mut() = Some(Arc::new(dem));
                    inner.terrain.set(Load::Ready);
                    Self::rebuild(&inner);
                }
                Ok(Err(e)) => {
                    tracing::warn!("Terrain could not be used: {e}");
                    inner.terrain.set(Load::Failed);
                }
                Err(_) => {
                    tracing::error!("Decoding the terrain panicked");
                    inner.terrain.set(Load::Failed);
                }
            }
            inner.update_status();
        });
    }

    fn start_map(this: &Rc<Self>, rt: &tokio::runtime::Handle, route: Route) {
        this.map.set(Load::Loading);
        let generation = this.generation.get();
        let weak = Rc::downgrade(this);
        crate::ui::spawn_to_main(rt, osm::load(osm::cache_dir(), route), move |loaded| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.generation.get() != generation {
                return;
            }
            match loaded {
                Ok(features) => {
                    *inner.features.borrow_mut() = Some(Arc::new(features));
                    inner.map.set(Load::Ready);
                    Self::rebuild(&inner);
                }
                Err(e) if e.downcast_ref::<osm::RouteTooComplex>().is_some() => {
                    tracing::warn!("No map data for this route: {e}");
                    inner.map.set(Load::Unavailable);
                }
                Err(e) => {
                    tracing::warn!("Map data did not load: {e:#}");
                    inner.map.set(Load::Failed);
                }
            }
            inner.update_status();
        });
    }

    /// Build the scene from the route and whatever has arrived, off the main
    /// thread — a long route takes long enough to stall a frame.
    fn rebuild(this: &Rc<Self>) {
        let Some(route) = this.route.borrow().clone() else {
            return;
        };
        let dem = this.dem.borrow().clone();
        let features = this.features.borrow().clone();
        let seq = this.build_seq.get() + 1;
        this.build_seq.set(seq);
        let generation = this.generation.get();
        this.update_status();
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            let (with_dem, with_map) = (dem.is_some(), features.is_some());
            let built = gtk::gio::spawn_blocking(move || {
                CourseScene::build_with(&route, dem.as_deref(), features.as_deref())
            })
            .await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.generation.get() != generation || inner.build_seq.get() != seq {
                return;
            }
            match built {
                Ok(scene) => {
                    let has_scene = scene.is_some();
                    *inner.scene.borrow_mut() = scene;
                    inner.needs_upload.set(true);
                    inner.area.queue_render();
                    match credit_text(with_dem && has_scene, with_map && has_scene) {
                        Some((text, sources)) => {
                            inner.credit.set_label(&credit_markup(&text));
                            inner.credit.set_tooltip_text(Some(&sources));
                            inner.credit.set_visible(true);
                        }
                        None => inner.credit.set_visible(false),
                    }
                }
                Err(_) => tracing::error!("Building the 3D course panicked"),
            }
            inner.update_status();
        });
    }

    /// Say what is happening, from where the scene and both downloads stand.
    fn update_status(&self) {
        let has_scene = self.scene.borrow().is_some()
            || self
                .route
                .borrow()
                .as_ref()
                .is_none_or(|r| r.points.len() < 2);
        match status_text(has_scene, self.terrain.get(), self.map.get()) {
            Some(t) => {
                self.status.set_label(t);
                self.status.set_visible(true);
            }
            None => self.status.set_visible(false),
        }
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
            state.upload(scene.as_ref().map(|s| &s.mesh));
        }

        let scale = area.scale_factor();
        let (w, h) = (area.width() * scale, area.height() * scale);
        let now = glib::monotonic_time();
        let distance = self.progress.get().distance_at(now);
        let pose = self
            .scene
            .borrow()
            .as_ref()
            .and_then(|s| course3d::camera_pose(&s.road, distance));

        state.draw(w, h, pose);
        self.drawn_m.set(pose.map(|_| distance));
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
                &[(0, "a_pos"), (1, "a_colour"), (2, "a_uv")],
            )?;
            let sky_program = link(&gl, header, SKY_VERTEX_SHADER, SKY_FRAGMENT_SHADER, &[])?;
            let sky_vao = gl.create_vertex_array()?;

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
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 28);
            gl.bind_vertex_array(None);

            Ok(Self {
                u_horizon: gl.get_uniform_location(sky_program, "u_horizon"),
                u_zenith: gl.get_uniform_location(sky_program, "u_zenith"),
                u_sun: gl.get_uniform_location(program, "u_sun"),
                sky_program,
                sky_vao,
                u_mvp: gl.get_uniform_location(program, "u_mvp"),
                u_eye: gl.get_uniform_location(program, "u_eye"),
                u_sky: gl.get_uniform_location(program, "u_sky"),
                u_fog_start: gl.get_uniform_location(program, "u_fog_start"),
                u_fog_end: gl.get_uniform_location(program, "u_fog_end"),
                gl,
                program,
                vao,
                vbo,
                ebo,
                index_count: 0,
                reported: Cell::new(false),
            })
        }
    }

    /// Replace what is on the GPU with `mesh`, or with nothing.
    fn upload(&mut self, mesh: Option<&Mesh>) {
        let (vertices, indices) = match mesh {
            Some(m) => (vertex_bytes(m), index_bytes(m)),
            None => (Vec::new(), Vec::new()),
        };
        unsafe {
            let gl = &self.gl;
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &vertices, glow::STATIC_DRAW);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ebo));
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, &indices, glow::STATIC_DRAW);
            gl.bind_vertex_array(None);
        }
        self.index_count = mesh.map_or(0, |m| m.indices.len() as i32);
    }

    fn draw(&self, width: i32, height: i32, pose: Option<course3d::CameraPose>) {
        let gl = &self.gl;
        // Logged on the first frame that has a scene to draw, which is the
        // one that matters if the rider sees only sky.
        let first = pose.is_some() && self.index_count > 0 && !self.reported.replace(true);
        if first {
            self.report_state(width, height, true);
        }
        unsafe {
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
            gl.clear_color(SKY_RGB[0], SKY_RGB[1], SKY_RGB[2], 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.use_program(Some(self.sky_program));
            gl.uniform_3_f32(self.u_horizon.as_ref(), SKY_RGB[0], SKY_RGB[1], SKY_RGB[2]);
            gl.uniform_3_f32(
                self.u_zenith.as_ref(),
                ZENITH_RGB[0],
                ZENITH_RGB[1],
                ZENITH_RGB[2],
            );
            gl.bind_vertex_array(Some(self.sky_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            let Some(pose) = pose else {
                return;
            };
            if self.index_count == 0 {
                return;
            }
            let aspect = width as f32 / height.max(1) as f32;
            let mvp = course3d::mul4(
                &course3d::perspective(FOV_DEG, aspect, NEAR_M, FAR_M),
                &course3d::look_at(pose.eye, pose.target),
            );
            gl.enable(glow::DEPTH_TEST);
            gl.depth_func(glow::LEQUAL);
            gl.use_program(Some(self.program));
            gl.uniform_matrix_4_f32_slice(self.u_mvp.as_ref(), false, &mvp);
            gl.uniform_3_f32(self.u_eye.as_ref(), pose.eye[0], pose.eye[1], pose.eye[2]);
            gl.uniform_3_f32(self.u_sky.as_ref(), SKY_RGB[0], SKY_RGB[1], SKY_RGB[2]);
            gl.uniform_3_f32(self.u_sun.as_ref(), SUN_DIR[0], SUN_DIR[1], SUN_DIR[2]);
            gl.uniform_1_f32(self.u_fog_start.as_ref(), FOG_START_M);
            gl.uniform_1_f32(self.u_fog_end.as_ref(), FOG_END_M);
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_elements(glow::TRIANGLES, self.index_count, glow::UNSIGNED_INT, 0);
            if first {
                let err = gl.get_error();
                tracing::info!(
                    "3D view first draw: {} indices, GL error 0x{err:X}",
                    self.index_count
                );
            }
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
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

    fn destroy(self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.sky_program);
            self.gl.delete_vertex_array(self.sky_vao);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_buffer(self.ebo);
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

/// Open each tile's PNG with GTK's image loader and decode its heights. Any
/// tile that will not open or decode fails the lot — see `terrain::load`.
fn decode_tiles(tiles: Vec<(TileId, Vec<u8>)>) -> Result<Dem, String> {
    let mut dem = Dem::default();
    for (id, bytes) in tiles {
        let texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
            .map_err(|e| format!("tile {id:?} is not an image: {e}"))?;
        let mut downloader = gtk::gdk::TextureDownloader::new(&texture);
        downloader.set_format(gtk::gdk::MemoryFormat::R8g8b8);
        let (rgb, stride) = downloader.download_bytes();
        let width = u32::try_from(texture.width()).unwrap_or(0);
        let height = u32::try_from(texture.height()).unwrap_or(0);
        let heights = terrain::decode_terrarium(&rgb, width, height, stride)
            .map_err(|e| format!("tile {id:?}: {e}"))?;
        dem.insert(id, heights);
    }
    Ok(dem)
}

fn vertex_bytes(mesh: &Mesh) -> Vec<u8> {
    mesh.vertices
        .iter()
        .flat_map(|v| v.pos.iter().chain(v.colour.iter()).chain(v.uv.iter()))
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
    fn should_say_it_is_building_before_there_is_anything_to_see() {
        assert_eq!(status_text(false, Load::Ready, Load::Ready), Some(BUILDING));
    }

    #[test]
    fn should_say_nothing_once_everything_is_in() {
        assert_eq!(status_text(true, Load::Ready, Load::Ready), None);
        // Before the view is shown nothing has been asked for: nothing to say.
        assert_eq!(status_text(true, Load::Wanted, Load::Wanted), None);
    }

    #[test]
    fn should_report_the_slow_map_download_only_after_terrain_is_in() {
        assert_eq!(
            status_text(true, Load::Loading, Load::Loading),
            Some("Loading terrain…")
        );
        let map = status_text(true, Load::Ready, Load::Loading).expect("a status");
        assert!(map.starts_with("Loading map data"), "{map}");
    }

    #[test]
    fn should_say_which_download_failed() {
        let t = status_text(true, Load::Failed, Load::Ready).expect("a status");
        assert!(t.starts_with("No terrain"), "{t}");
        let m = status_text(true, Load::Ready, Load::Failed).expect("a status");
        assert!(m.starts_with("No map data"), "{m}");
    }

    #[test]
    fn should_retry_a_failure_but_never_an_impossible_route() {
        assert!(Load::Wanted.should_start());
        assert!(Load::Failed.should_start());
        assert!(!Load::Loading.should_start());
        assert!(!Load::Ready.should_start());
        assert!(!Load::Unavailable.should_start());
    }

    #[test]
    fn should_credit_openstreetmap_whenever_its_data_is_shown() {
        let (text, _) = credit_text(false, true).expect("a credit");
        assert!(text.contains("© OpenStreetMap contributors"));
        let (text, sources) = credit_text(true, true).expect("a credit");
        assert!(text.contains("© OpenStreetMap contributors") && text.contains("Terrain Tiles"));
        assert!(sources.contains("Open Database License"));
        let (text, _) = credit_text(true, false).expect("a credit");
        assert!(!text.contains("OpenStreetMap"));
        assert_eq!(credit_text(false, false), None);
    }

    #[test]
    fn should_link_the_openstreetmap_credit_to_its_licence() {
        let (text, _) = credit_text(true, true).expect("a credit");
        let markup = credit_markup(&text);
        assert!(markup.contains(r#"<a href="https://www.openstreetmap.org/copyright">"#));
        // The elevation credit stays plain text.
        assert!(markup.contains("Terrain Tiles"));
        assert_eq!(credit_markup("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn should_pack_nine_floats_per_vertex() {
        let mesh = Mesh {
            vertices: vec![course3d::Vertex {
                pos: [1.0, 2.0, 3.0],
                colour: [0.1, 0.2, 0.3, 1.0],
                uv: [7.0, 8.0],
            }],
            indices: vec![0, 0, 0],
        };
        let bytes = vertex_bytes(&mesh);
        assert_eq!(bytes.len(), std::mem::size_of::<course3d::Vertex>());
        assert_eq!(&bytes[12..16], &0.1f32.to_ne_bytes());
        assert_eq!(
            &bytes[28..32],
            &7.0f32.to_ne_bytes(),
            "uv after colour, at the offset GL reads"
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
/// CYCLE_3D_GPX=route.gpx CYCLE_3D_OUT=/tmp/view.png [CYCLE_3D_TILES=<terrain cache>] \
///   [CYCLE_3D_AT=<metres>] cargo test render_course_offscreen -- --ignored --nocapture
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
        // CYCLE_3D_OSM=<saved Overpass answer> adds buildings, land use, water.
        let features = std::env::var("CYCLE_3D_OSM")
            .ok()
            .map(|path| osm::parse(&std::fs::read(path).expect("the map file")).expect("map data"));
        let scene = match std::env::var("CYCLE_3D_TILES") {
            Ok(root) => {
                let ids = terrain::tiles_for_route(&route, terrain::CORRIDOR_M).expect("tiles");
                let tiles: Vec<(TileId, Vec<u8>)> = ids
                    .into_iter()
                    .map(|id| {
                        let bytes = terrain::read_cached(std::path::Path::new(&root), id)
                            .unwrap_or_else(|| panic!("{id:?} not cached"));
                        (id, bytes)
                    })
                    .collect();
                let dem = decode_tiles(tiles).expect("decodes");
                CourseScene::build_with(&route, Some(&dem), features.as_ref()).expect("a scene")
            }
            Err(_) => CourseScene::build_with(&route, None, features.as_ref()).expect("a scene"),
        };
        eprintln!("scene built in {:.2} s", started.elapsed().as_secs_f64());
        let pose = course3d::camera_pose(&scene.road, at).expect("a pose");
        eprintln!(
            "{} vertices, {} indices; eye {:?} target {:?}",
            scene.mesh.vertices.len(),
            scene.mesh.indices.len(),
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
            state.upload(Some(&scene.mesh));
            // CYCLE_3D_GTK_STATE=1 leaves the context as GTK's renderer
            // leaves it — depth cleared to 0, depth test on — which once
            // showed the rider nothing but sky.
            if std::env::var("CYCLE_3D_GTK_STATE").is_ok_and(|v| v == "1") {
                state.gl.clear_depth_f32(0.0);
                state.gl.enable(glow::DEPTH_TEST);
                state.gl.depth_func(glow::GREATER);
            }
            state.draw(W, H, Some(pose));
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
