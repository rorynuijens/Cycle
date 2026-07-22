use adw::prelude::*;

use crate::data::athlete::ZONE_COLORS;
use crate::data::workout::WorkoutCategory;

/// Map a workout category to the zone colour of its dominant intensity.
/// Zone RGB via Cairo is the app's one expressive colour (CLAUDE.md §1.6).
/// `None` = no category colour (draw neutral instead).
pub fn category_zone_rgb(cat: &WorkoutCategory) -> Option<(f64, f64, f64)> {
    match cat {
        WorkoutCategory::Recovery => Some(ZONE_COLORS[0]),
        WorkoutCategory::Endurance => Some(ZONE_COLORS[1]),
        WorkoutCategory::Tempo => Some(ZONE_COLORS[2]),
        WorkoutCategory::SweetSpot => Some(ZONE_COLORS[2]),
        WorkoutCategory::Threshold => Some(ZONE_COLORS[3]),
        WorkoutCategory::Vo2Max => Some(ZONE_COLORS[4]),
        WorkoutCategory::Anaerobic => Some(ZONE_COLORS[5]),
        WorkoutCategory::Custom => None,
    }
}

/// Slim vertical colour stripe marking a row's intensity category.
/// `None` renders a neutral foreground-tinted stripe (theme-aware).
pub fn color_stripe(rgb: Option<(f64, f64, f64)>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_width(6).build();
    area.set_draw_func(move |widget, cr, width, height| {
        let (r, g, b, a) = match rgb {
            Some((r, g, b)) => (r, g, b, 1.0),
            None => {
                let fg = widget.color();
                (fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.3)
            }
        };
        cr.set_source_rgba(r, g, b, a);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        cr.fill().ok();
    });
    area
}

/// Small colour swatch square for legends and section headings.
pub fn zone_swatch(rgb: (f64, f64, f64)) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(12)
        .content_height(12)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |_widget, cr, width, height| {
        let (r, g, b) = rgb;
        cr.set_source_rgba(r, g, b, 0.85);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        cr.fill().ok();
    });
    area
}
