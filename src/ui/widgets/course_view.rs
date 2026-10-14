//! The road from the saddle: a 3D view of the route, drawn with OpenGL.
//!
//! Everything it draws comes from [`crate::data::course3d`]; this module only
//! uploads that geometry and moves the camera. The ride loop reports distance
//! once a second, and a camera that jumped once a second would be unwatchable,
//! so between reports the view carries the rider on at their last speed —
//! for at most a second, so a paused ride stops instead of driving on.
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
use crate::data::route::Route;

/// Vertical field of view, in degrees. Roughly what a rider takes in looking
/// down the road, and wide enough that a bend does not leave the frame.
const FOV_DEG: f32 = 60.0;
const NEAR_M: f32 = 0.3;
const FAR_M: f32 = 2000.0;

/// Haze: geometry fades into the sky between these distances. It is what
/// hides the edge of the ground, so the far end must sit inside its reach.
const FOG_START_M: f32 = 150.0;
const FOG_END_M: f32 = 560.0;

/// Sky and haze colour, linear RGB. Fixed, like the scene — see `course3d`.
const SKY_RGB: [f32; 3] = [0.72, 0.81, 0.90];

/// Longest the view will carry the rider on without a fresh distance, in
/// seconds. A tick is due every second; past that, the ride has paused.
const MAX_EXTRAPOLATE_S: f32 = 1.0;

/// A camera that moved less than this since the last frame is not redrawn.
const REDRAW_EPSILON_M: f32 = 0.001;

const VERTEX_SHADER: &str = r#"
in vec3 a_pos;
in vec4 a_colour;
uniform mat4 u_mvp;
out vec3 v_world;
out vec4 v_colour;
void main() {
    v_world = a_pos;
    v_colour = a_colour;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"
in vec3 v_world;
in vec4 v_colour;
uniform vec3 u_eye;
uniform vec3 u_sky;
uniform float u_fog_start;
uniform float u_fog_end;
out vec4 frag;

float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
    vec3 c = v_colour.rgb;
    // Ground: two scales of mottling, so the grass slides past and the eye
    // can read speed from it.
    if (v_colour.a > 0.5) {
        float fine = hash(floor(v_world.xz / 2.0));
        float coarse = hash(floor(v_world.xz / 14.0));
        c *= 0.84 + 0.16 * fine + 0.12 * (coarse - 0.5);
    }
    float d = distance(v_world, u_eye);
    float f = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    frag = vec4(mix(c, u_sky, f), 1.0);
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
}

/// Everything living in the GL context. Created on realize, dropped on
/// unrealize — the context and everything in it go together.
struct GlState {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    index_count: i32,
    u_mvp: Option<glow::UniformLocation>,
    u_eye: Option<glow::UniformLocation>,
    u_sky: Option<glow::UniformLocation>,
    u_fog_start: Option<glow::UniformLocation>,
    u_fog_end: Option<glow::UniformLocation>,
}

struct Inner {
    area: gtk::GLArea,
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

        let inner = Rc::new(Inner {
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

    pub fn widget(&self) -> &gtk::GLArea {
        &self.inner.area
    }

    /// Show `route`, with the rider at its start.
    ///
    /// The scene is built off the main thread — a long route's ground grid
    /// takes long enough to stall a frame — and handed back here to upload.
    pub fn set_route(&self, route: &Route) {
        let generation = self.inner.generation.get() + 1;
        self.inner.generation.set(generation);
        *self.inner.scene.borrow_mut() = None;
        self.inner.needs_upload.set(true);
        self.inner.progress.set(Progress::default());
        self.inner.drawn_m.set(None);

        let route = route.clone();
        let weak = Rc::downgrade(&self.inner);
        glib::spawn_future_local(async move {
            let built = gtk::gio::spawn_blocking(move || CourseScene::build(&route)).await;
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
        });
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
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, src) in [
                (glow::VERTEX_SHADER, VERTEX_SHADER),
                (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
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
            gl.bind_attrib_location(program, 0, "a_pos");
            gl.bind_attrib_location(program, 1, "a_colour");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }

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

            Ok(Self {
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
        unsafe {
            gl.viewport(0, 0, width, height);
            gl.clear_color(SKY_RGB[0], SKY_RGB[1], SKY_RGB[2], 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
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
            gl.uniform_1_f32(self.u_fog_start.as_ref(), FOG_START_M);
            gl.uniform_1_f32(self.u_fog_end.as_ref(), FOG_END_M);
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_elements(glow::TRIANGLES, self.index_count, glow::UNSIGNED_INT, 0);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
        }
    }

    fn destroy(self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_buffer(self.ebo);
        }
    }
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

    #[test]
    fn should_pack_seven_floats_per_vertex() {
        let mesh = Mesh {
            vertices: vec![course3d::Vertex {
                pos: [1.0, 2.0, 3.0],
                colour: [0.1, 0.2, 0.3, 1.0],
            }],
            indices: vec![0, 0, 0],
        };
        let bytes = vertex_bytes(&mesh);
        assert_eq!(bytes.len(), std::mem::size_of::<course3d::Vertex>());
        assert_eq!(&bytes[12..16], &0.1f32.to_ne_bytes());
        assert_eq!(index_bytes(&mesh).len(), 12);
    }
}
