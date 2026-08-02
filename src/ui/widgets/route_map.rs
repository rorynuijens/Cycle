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

/// Zoom level used when following the rider — close enough to see the road,
/// wide enough to see the next corner coming.
const FOLLOW_ZOOM: f64 = 14.0;

pub struct RouteMap {
    map: libshumate::SimpleMap,
    path_layer: RefCell<Option<libshumate::PathLayer>>,
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
        path_layer.set_stroke_color(Some(&gtk::gdk::RGBA::new(0.35, 0.60, 1.0, 0.9)));
        path_layer.set_stroke_width(3.0);
        self.map.add_overlay_layer(&path_layer);
        *self.path_layer.borrow_mut() = Some(path_layer);

        self.frame_route(points, &viewport);
        self.last_interaction.set(None);
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
            let dot = gtk::Image::from_icon_name("find-location-symbolic");
            dot.add_css_class("accent");
            dot.set_pixel_size(20);
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
