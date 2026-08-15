//! Slippy map showing a route and, during a ride, where the rider is on it.
//!
//! Shared by the live route player and the post-ride detail dialog so the tile
//! source, styling and attribution are configured in one place.
//!
//! Tiles come from the network. Offline, the base map is blank but the route
//! line and rider marker still draw, so the map degrades rather than breaks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use libshumate::prelude::*;

/// How long the map stops following the rider after the rider pans or zooms it.
///
/// Recentring on every tick would drag the view back mid-gesture, which makes
/// looking ahead up the road impossible.
const FOLLOW_SUSPEND: Duration = Duration::from_secs(10);

/// Zoom level used when following the rider — street level, roughly 400 m
/// across, so individual roads and the next junction are readable at speed.
///
/// The wider town-scale view this used to sit at showed where the ride was
/// happening but not where the road went, which is the one thing a rider needs
/// from a map mid-effort.
const FOLLOW_ZOOM: f64 = 16.0;

/// Width of the route line, and of the pale casing drawn under it.
///
/// OSM's own tiles are full of coloured roads, so a thin translucent line
/// disappears into them. A wide line with a contrasting casing is the standard
/// cartographic answer and reads at a glance without hiding the road beneath.
const ROUTE_STROKE_WIDTH: f64 = 7.0;
const ROUTE_OUTLINE_WIDTH: f64 = 3.0;

/// Width of the highlight over the stretch the rider is about to ride.
const LOOKAHEAD_STROKE_WIDTH: f64 = 9.0;

/// Map colours are fixed rather than theme-derived on purpose: OSM raster tiles
/// are light whatever the app theme is doing, so a line that followed the theme
/// would turn near-black-on-grey in dark mode and read worse, not better. This
/// is map cartography, not app chrome (CLAUDE.md §1.6 governs the latter).
const ROUTE_COLOR: gtk::gdk::RGBA = gtk::gdk::RGBA::new(0.13, 0.35, 0.85, 1.0);
const ROUTE_CASING_COLOR: gtk::gdk::RGBA = gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 0.9);
/// The next kilometre, in a warm colour no OSM road uses.
const LOOKAHEAD_COLOR: gtk::gdk::RGBA = gtk::gdk::RGBA::new(1.0, 0.42, 0.05, 0.95);

/// Overall size of the rider marker, in pixels — the dot plus its halo.
const MARKER_SIZE: i32 = 34;

pub struct RouteMap {
    map: libshumate::SimpleMap,
    path_layer: RefCell<Option<libshumate::PathLayer>>,
    /// The next stretch of road, drawn over the route line. Rebuilt each tick,
    /// which is why it is kept separate from the full route: the route may hold
    /// thousands of nodes and must never be rebuilt mid-ride.
    lookahead_layer: RefCell<Option<libshumate::PathLayer>>,
    marker_layer: RefCell<Option<libshumate::MarkerLayer>>,
    marker: RefCell<Option<libshumate::Marker>>,
    /// When the rider last panned or zoomed; following resumes after that.
    /// Shared with the gesture controllers that record it.
    last_interaction: Rc<Cell<Option<Instant>>>,
}

impl RouteMap {
    pub fn new() -> Self {
        let map = libshumate::SimpleMap::new();
        map.set_hexpand(true);
        map.set_vexpand(true);
        map.set_map_source(Some(&libshumate::RasterRenderer::from_url(
            "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
        )));
        map.set_tooltip_text(Some(
            "Route map — drag to look around; following resumes automatically",
        ));

        let last_interaction: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        watch_for_interaction(&map, &last_interaction);

        Self {
            map,
            path_layer: RefCell::new(None),
            lookahead_layer: RefCell::new(None),
            marker_layer: RefCell::new(None),
            marker: RefCell::new(None),
            last_interaction,
        }
    }

    pub fn widget(&self) -> &libshumate::SimpleMap {
        &self.map
    }

    /// True when the rider has not touched the map recently.
    fn may_follow(&self) -> bool {
        match self.last_interaction.get() {
            Some(at) => at.elapsed() >= FOLLOW_SUSPEND,
            None => true,
        }
    }

    /// Draw `points` (latitude, longitude) as the route line, replacing any
    /// previous one, and frame the whole route.
    pub fn set_route(&self, points: &[(f64, f64)]) {
        let Some(viewport) = self.map.viewport() else {
            return;
        };
        if let Some(old) = self.path_layer.borrow_mut().take() {
            self.map.remove_overlay_layer(&old);
        }
        if points.is_empty() {
            return;
        }

        let path_layer = libshumate::PathLayer::new(&viewport);
        for &(lat, lng) in points {
            path_layer.add_node(&libshumate::Coordinate::new_full(lat, lng));
        }
        path_layer.set_stroke_color(Some(&ROUTE_COLOR));
        path_layer.set_stroke_width(ROUTE_STROKE_WIDTH);
        path_layer.set_outline_color(Some(&ROUTE_CASING_COLOR));
        path_layer.set_outline_width(ROUTE_OUTLINE_WIDTH);
        self.map.add_overlay_layer(&path_layer);
        *self.path_layer.borrow_mut() = Some(path_layer);

        self.frame_route(points, &viewport);
        self.last_interaction.set(None);
    }

    /// Highlight the stretch of road the rider is about to ride.
    ///
    /// The route line says where the course goes; this says which way to go
    /// *next*, which at a junction is a different question. Called every tick
    /// with a short slice — around twenty nodes — so rebuilding it is cheap.
    /// Passing an empty slice clears the highlight.
    pub fn set_lookahead(&self, points: &[(f64, f64)]) {
        let Some(viewport) = self.map.viewport() else {
            return;
        };

        // Created lazily so a static map that never rides keeps one layer.
        if self.lookahead_layer.borrow().is_none() {
            let layer = libshumate::PathLayer::new(&viewport);
            layer.set_stroke_color(Some(&LOOKAHEAD_COLOR));
            layer.set_stroke_width(LOOKAHEAD_STROKE_WIDTH);
            self.map.add_overlay_layer(&layer);
            *self.lookahead_layer.borrow_mut() = Some(layer);
        }

        if let Some(layer) = self.lookahead_layer.borrow().as_ref() {
            layer.remove_all();
            for &(lat, lng) in points {
                layer.add_node(&libshumate::Coordinate::new_full(lat, lng));
            }
        }
    }

    /// Centre and zoom so the whole route is visible.
    fn frame_route(&self, points: &[(f64, f64)], viewport: &libshumate::Viewport) {
        let (mut min_lat, mut max_lat) = (f64::MAX, f64::MIN);
        let (mut min_lng, mut max_lng) = (f64::MAX, f64::MIN);
        for &(lat, lng) in points {
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
            min_lng = min_lng.min(lng);
            max_lng = max_lng.max(lng);
        }
        viewport.set_location((min_lat + max_lat) / 2.0, (min_lng + max_lng) / 2.0);
        // Same span-to-zoom estimate the ride detail dialog uses.
        let span = (max_lat - min_lat).max(max_lng - min_lng).max(1e-6);
        viewport.set_zoom_level(((360.0 / span).log2() - 1.0).clamp(2.0, 16.0));
    }

    /// Move the rider marker, creating it on first use, and follow it unless the
    /// rider is currently looking around the map.
    pub fn set_position(&self, lat: f64, lng: f64) {
        let Some(viewport) = self.map.viewport() else {
            return;
        };

        if self.marker.borrow().is_none() {
            let layer = libshumate::MarkerLayer::new(&viewport);
            let marker = libshumate::Marker::new();
            let dot = rider_dot();
            marker.set_child(Some(&dot));
            marker.update_property(&[gtk::accessible::Property::Label("Your position")]);
            layer.add_marker(&marker);
            self.map.add_overlay_layer(&layer);
            *self.marker_layer.borrow_mut() = Some(layer);
            *self.marker.borrow_mut() = Some(marker);
            // The first fix is worth centring on whatever the rider was doing.
            viewport.set_zoom_level(FOLLOW_ZOOM);
            viewport.set_location(lat, lng);
        }

        if let Some(marker) = self.marker.borrow().as_ref() {
            marker.set_location(lat, lng);
        }
        if self.may_follow() {
            viewport.set_location(lat, lng);
        }
    }
}

/// The rider's position on the map: a solid accent core inside a white ring
/// inside a soft halo.
///
/// This replaced a 20 px location-pin icon, which sat at roughly the weight of
/// an OSM place marker and was lost among them. The ring is what does the work —
/// a coloured dot alone competes with the map's own colours, while a dot with a
/// light collar reads against anything underneath it.
///
/// The accent colour is picked up the way the rest of the app's charts do it:
/// the `accent` CSS class makes `widget.color()` resolve to the GNOME accent,
/// since libadwaita 1.5 exposes no accent API.
fn rider_dot() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(MARKER_SIZE)
        .content_height(MARKER_SIZE)
        .css_classes(["accent"])
        .can_target(false) // never swallow a drag meant for the map
        .build();

    area.set_draw_func(|widget, cr, width, height| {
        let accent = widget.color();
        let (r, g, b) = (
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
        );
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        let radius = cx.min(cy);

        // Halo — gives the marker presence without hiding the road under it.
        cr.set_source_rgba(r, g, b, 0.22);
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        cr.fill().ok();

        // White collar.
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.arc(cx, cy, radius * 0.62, 0.0, std::f64::consts::TAU);
        cr.fill().ok();

        // Core.
        cr.set_source_rgba(r, g, b, 1.0);
        cr.arc(cx, cy, radius * 0.44, 0.0, std::f64::consts::TAU);
        cr.fill().ok();
    });

    area
}

/// Note pans and zooms so following stands aside while the rider looks around.
///
/// The controllers run in the capture phase, so they observe the gesture without
/// taking it away from the map itself.
fn watch_for_interaction(map: &libshumate::SimpleMap, last: &Rc<Cell<Option<Instant>>>) {
    let drag = gtk::GestureDrag::new();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let drag_last = Rc::clone(last);
    drag.connect_drag_begin(move |_, _, _| drag_last.set(Some(Instant::now())));
    map.add_controller(drag);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let scroll_last = Rc::clone(last);
    scroll.connect_scroll(move |_, _, _| {
        scroll_last.set(Some(Instant::now()));
        glib::Propagation::Proceed
    });
    map.add_controller(scroll);
}
