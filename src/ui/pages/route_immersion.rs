//! Fullscreen on the 3D road: the route page gives the whole screen to the 3D
//! view, which draws every reading itself as a HUD in the world's style
//! (`ui/widgets/ride_hud.rs`). The page's own controls move into a bar over
//! the road that slides up when the mouse moves and away again when it rests.
//!
//! The 3D view is never moved: its GL context, and the world on it, stay put.
//! Only what is around it hides — whole sections, so a row that shows and
//! hides itself mid-ride (a course cue, a device chip) cannot pop back in.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::ui::widgets::course_view::CourseView;
use crate::ui::widgets::ride_hud;
use crate::ui::{FULLSCREEN_CLAMP, WINDOWED_CLAMP};

/// How long the controls stay up after the mouse last moved.
const CONTROLS_LINGER: Duration = Duration::from_secs(3);
/// The pointer must move this far, in pixels, to count as the rider reaching
/// for the mouse. GTK also reports motion when what is under a resting
/// pointer moves — the bar sliding away under it — and that must not bring
/// the bar straight back.
const MOVE_PX: f64 = 3.0;
/// Wide enough that the clamp never holds the road in: the view fills any
/// screen.
const UNCLAMPED: i32 = 100_000;

/// Which of the page's three layouts is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Windowed,
    /// Fullscreen with the map: the cockpit, wider.
    Cockpit,
    /// Fullscreen on the 3D road: the road and the HUD, nothing else.
    Road,
}

/// The layout for a window that is or is not `fullscreen`, showing the 3D
/// road or the map.
/// Whether the pointer, last seen at `last`, has really moved to `(x, y)`.
pub fn moved_enough(last: Option<(f64, f64)>, x: f64, y: f64) -> bool {
    match last {
        None => true,
        Some((lx, ly)) => (x - lx).hypot(y - ly) >= MOVE_PX,
    }
}

pub fn layout(fullscreen: bool, road: bool) -> Layout {
    match (fullscreen, road) {
        (false, _) => Layout::Windowed,
        (true, false) => Layout::Cockpit,
        (true, true) => Layout::Road,
    }
}

/// The parts of the route page that change with the layout.
pub struct Immersion {
    pub clamp: adw::Clamp,
    pub inner: gtk::Box,
    /// The cockpit above and below the map slot.
    pub above: gtk::Box,
    pub below: gtk::Box,
    pub lookahead: gtk::DrawingArea,
    /// The pause, intensity, view and end controls: in the page normally,
    /// in the bar over the road on [`Layout::Road`].
    pub controls: gtk::Box,
    pub course_view: CourseView,
    /// The bar over the road, and the box in it the controls move into.
    bar: gtk::Revealer,
    bar_box: gtk::Box,
    fullscreen: Cell<bool>,
    road: Cell<bool>,
    current: Cell<Option<Layout>>,
    /// Bumped by every mouse move: a hide timer from an earlier move finds
    /// it changed and leaves the controls up.
    moved: Cell<u64>,
    /// The pointer is on the bar: it stays up.
    on_bar: Cell<bool>,
    /// Where the pointer was last seen over the road.
    pointer: Cell<Option<(f64, f64)>>,
    /// Over the line: the finished card has the road, and the bar stays down
    /// — its button row is where the bar would come up.
    finished: Cell<bool>,
}

impl Immersion {
    pub fn new(
        clamp: adw::Clamp,
        inner: gtk::Box,
        above: gtk::Box,
        below: gtk::Box,
        lookahead: gtk::DrawingArea,
        controls: gtk::Box,
        course_view: CourseView,
    ) -> Rc<Self> {
        let bar_box = gtk::Box::builder().css_classes(["toolbar", "osd"]).build();
        let bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .reveal_child(false)
            .child(&bar_box)
            .build();
        course_view.widget().add_overlay(&bar);

        let this = Rc::new(Self {
            clamp,
            inner,
            above,
            below,
            lookahead,
            controls,
            course_view,
            bar,
            bar_box,
            fullscreen: Cell::new(false),
            road: Cell::new(false),
            current: Cell::new(None),
            moved: Cell::new(0),
            on_bar: Cell::new(false),
            pointer: Cell::new(None),
            finished: Cell::new(false),
        });

        // Held weakly: the view owns these controllers (CLAUDE.md §2.4).
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_motion(move |_, x, y| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if moved_enough(this.pointer.get(), x, y) {
                this.pointer.set(Some((x, y)));
                Self::show_controls(&this);
            }
        });
        this.course_view.widget().add_controller(motion);
        let on_bar = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        on_bar.connect_enter(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.on_bar.set(true);
            }
        });
        let weak = Rc::downgrade(&this);
        on_bar.connect_leave(move |_| {
            // Leaving the bar — or the bar sliding out from under the
            // pointer — only restarts the wait; it never brings the bar up.
            if let Some(this) = weak.upgrade() {
                this.on_bar.set(false);
                if this.bar.reveals_child() {
                    this.moved.set(this.moved.get() + 1);
                    Self::hide_later(&this);
                }
            }
        });
        this.bar.add_controller(on_bar);
        Self::apply(&this);
        this
    }

    pub fn set_fullscreen(this: &Rc<Self>, fullscreen: bool) {
        this.fullscreen.set(fullscreen);
        Self::apply(this);
    }

    /// The 3D road is showing in the map's slot, or the map is.
    pub fn set_road(this: &Rc<Self>, road: bool) {
        this.road.set(road);
        Self::apply(this);
    }

    /// The ride is over, or a new one is starting: keep the bar down while
    /// the finished card is up, the pointer showing to press its button.
    pub fn set_finished(&self, finished: bool) {
        self.finished.set(finished);
        if finished {
            self.bar.set_reveal_child(false);
            self.course_view.widget().set_cursor(None);
        }
    }

    /// A toggle handler for the 3D button that does not keep the page alive.
    pub fn road_toggle(this: &Rc<Self>) -> impl Fn(&gtk::ToggleButton) + 'static {
        let weak: Weak<Self> = Rc::downgrade(this);
        move |btn| {
            if let Some(this) = weak.upgrade() {
                Self::set_road(&this, btn.is_active());
            }
        }
    }

    fn apply(this: &Rc<Self>) {
        let s = this.as_ref();
        let want = layout(s.fullscreen.get(), s.road.get());
        let road = want == Layout::Road;
        let was_road = s.current.get() == Some(Layout::Road);
        s.current.set(Some(want));

        let (clamp_max, margin, top, spacing) = match want {
            Layout::Windowed => (WINDOWED_CLAMP, 24, 20, 18),
            Layout::Cockpit => (FULLSCREEN_CLAMP, 12, 12, 12),
            Layout::Road => (UNCLAMPED, 0, 0, 0),
        };
        s.clamp.set_maximum_size(clamp_max);
        s.clamp.set_tightening_threshold(clamp_max);
        s.clamp.set_margin_top(top);
        s.clamp.set_margin_bottom(margin);
        s.clamp.set_margin_start(margin);
        s.clamp.set_margin_end(margin);
        for b in [&s.inner, &s.above, &s.below] {
            b.set_spacing(spacing);
        }
        s.above.set_visible(!road);
        s.below.set_visible(!road);
        s.lookahead.set_visible(!road);
        s.course_view.set_hud_shown(road);

        if road && !was_road {
            s.inner.remove(&s.controls);
            s.bar_box.append(&s.controls);
            s.bar.set_reveal_child(true);
            s.moved.set(s.moved.get() + 1);
            Self::hide_later(this);
        } else if !road && was_road {
            s.bar_box.remove(&s.controls);
            s.inner.append(&s.controls);
            s.bar.set_reveal_child(false);
            s.course_view.widget().set_cursor(None);
        }
    }

    /// Bring the controls up — clear of the HUD's profile band — and the
    /// pointer back, for a few seconds.
    fn show_controls(this: &Rc<Self>) {
        if this.current.get() != Some(Layout::Road) {
            return;
        }
        let view = this.course_view.widget();
        if this.finished.get() {
            view.set_cursor(None);
            return;
        }
        let scale = f64::from(view.scale_factor().max(1));
        let clear = ride_hud::controls_clearance(
            (f64::from(view.width()) * scale) as i32,
            (f64::from(view.height()) * scale) as i32,
        ) / scale;
        this.bar.set_margin_bottom(clear.round() as i32);
        this.bar.set_reveal_child(true);
        view.set_cursor(None);
        this.moved.set(this.moved.get() + 1);
        Self::hide_later(this);
    }

    /// Hide the controls and the pointer once the mouse has rested.
    fn hide_later(this: &Rc<Self>) {
        let ticket = this.moved.get();
        let weak = Rc::downgrade(this);
        glib::timeout_add_local_once(CONTROLS_LINGER, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.moved.get() != ticket
                || this.on_bar.get()
                || this.finished.get()
                || this.current.get() != Some(Layout::Road)
            {
                return;
            }
            this.bar.set_reveal_child(false);
            this.course_view.widget().set_cursor_from_name(Some("none"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_count_only_a_real_move_of_the_pointer() {
        assert!(moved_enough(None, 10.0, 10.0));
        assert!(!moved_enough(Some((10.0, 10.0)), 10.0, 10.0));
        assert!(!moved_enough(Some((10.0, 10.0)), 12.0, 11.0));
        assert!(moved_enough(Some((10.0, 10.0)), 13.0, 10.0));
        assert!(moved_enough(Some((10.0, 10.0)), 10.0, 6.0));
    }

    #[test]
    fn should_give_the_road_the_screen_only_when_fullscreen_on_the_3d_view() {
        assert_eq!(layout(false, false), Layout::Windowed);
        assert_eq!(layout(false, true), Layout::Windowed);
        assert_eq!(layout(true, false), Layout::Cockpit);
        assert_eq!(layout(true, true), Layout::Road);
    }
}
