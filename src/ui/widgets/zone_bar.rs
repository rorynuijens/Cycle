//! A stacked proportional bar showing how training time splits across zones.
//!
//! Used for both power zones (Z1–Z7) and heart-rate zones (Z1–Z5) — the two
//! share the app's single zone palette, per the zone-colour design language.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::ZONE_COLORS;

/// Height of the bar in pixels — a thin strip, read as a proportion rather
/// than a chart, so it needs no axis.
const BAR_HEIGHT: i32 = 20;

/// One drawn segment: horizontal offset, width, and the zone it represents.
type Segment = (f64, f64, usize);

/// Lay out the bar's segments across `width`, skipping zones with no time.
///
/// Returns an empty vector when nothing has been recorded, which the draw
/// function treats as "draw nothing" rather than "draw an empty bar".
/// Zone indices beyond the palette clamp to the last colour.
fn segments(seconds: &[u32], width: f64) -> Vec<Segment> {
    let total: u32 = seconds.iter().sum();
    if total == 0 {
        return Vec::new();
    }

    let mut x = 0.0f64;
    let mut out = Vec::new();
    for (i, &secs) in seconds.iter().enumerate() {
        if secs == 0 {
            continue;
        }
        let seg_w = (secs as f64 / total as f64) * width;
        out.push((x, seg_w, i.min(ZONE_COLORS.len() - 1)));
        x += seg_w;
    }
    out
}

/// A stacked zone-distribution bar backed by per-zone second counts.
#[derive(Clone)]
pub struct ZoneBar {
    area: gtk::DrawingArea,
    seconds: Rc<RefCell<Vec<u32>>>,
}

impl ZoneBar {
    /// Build an empty bar. `accessible_label` is the screen-reader description
    /// of what the bar shows — required, since the bar is purely visual.
    pub fn new(accessible_label: &str) -> Self {
        let seconds: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));

        let area = gtk::DrawingArea::builder()
            .content_height(BAR_HEIGHT)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        area.update_property(&[gtk::accessible::Property::Label(accessible_label)]);

        let data = Rc::clone(&seconds);
        area.set_draw_func(move |_widget, cr, width, height| {
            let seconds = data.borrow();
            for (x, seg_w, zone) in segments(&seconds, width as f64) {
                let (r, g, b) = ZONE_COLORS[zone];
                cr.set_source_rgba(r, g, b, 0.85);
                cr.rectangle(x, 0.0, seg_w, height as f64);
                cr.fill().ok();
            }
        });

        Self { area, seconds }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Replace the bar's data and redraw.
    pub fn set_seconds(&self, seconds: &[u32]) {
        *self.seconds.borrow_mut() = seconds.to_vec();
        self.area.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_draw_nothing_when_no_time_is_recorded() {
        assert!(segments(&[0, 0, 0], 100.0).is_empty());
    }

    #[test]
    fn should_draw_nothing_when_there_are_no_zones_at_all() {
        assert!(segments(&[], 100.0).is_empty());
    }

    #[test]
    fn should_give_one_full_width_segment_when_all_time_is_in_one_zone() {
        let segs = segments(&[0, 600, 0], 100.0);
        assert_eq!(segs, vec![(0.0, 100.0, 1)]);
    }

    #[test]
    fn should_split_width_in_proportion_to_time() {
        let segs = segments(&[300, 900], 100.0);
        assert_eq!(segs, vec![(0.0, 25.0, 0), (25.0, 75.0, 1)]);
    }

    #[test]
    fn should_skip_empty_zones_without_leaving_a_gap() {
        // Z1 and Z3 ridden, Z2 not — the two segments must still be adjacent.
        let segs = segments(&[100, 0, 100], 80.0);
        assert_eq!(segs, vec![(0.0, 40.0, 0), (40.0, 40.0, 2)]);
    }

    #[test]
    fn should_fill_the_full_width_across_all_segments() {
        let segs = segments(&[7, 11, 3, 29], 640.0);
        let drawn: f64 = segs.iter().map(|&(_, w, _)| w).sum();
        assert!((drawn - 640.0).abs() < 1e-9, "segments must tile the bar");
    }

    #[test]
    fn should_clamp_zone_index_to_the_palette() {
        // A caller passing more buckets than the palette has must not panic.
        let secs = [1u32; 9];
        let segs = segments(&secs, 90.0);
        assert_eq!(segs.len(), 9);
        assert_eq!(segs.last().expect("9 segments").2, ZONE_COLORS.len() - 1);
    }
}
