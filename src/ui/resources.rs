use adw::prelude::*;
use std::f64::consts::PI;

/// Load the bundled RPE emoticon thumbnail for `rpe` (1–6) from GLib resources.
/// Returns `None` if the resource is missing or the bytes cannot be decoded as a texture.
pub fn rpe_texture(rpe: u8) -> Option<gtk::gdk::Texture> {
    let path = format!("/io/github/rorynuijens/Cycle/thumbnails/rpe_{rpe}.png");
    let bytes =
        gio::functions::resources_lookup_data(&path, gio::ResourceLookupFlags::NONE).ok()?;
    gtk::gdk::Texture::from_bytes(&bytes).ok()
}

/// Return a small sport-type icon drawn with Cairo for use in calendar and history views.
///
/// `sport_type` is the raw string from Intervals.icu (e.g. "Ride", "Run", "VirtualRide").
/// `is_indoor` overrides the cycling icon to the stationary-trainer variant.
/// CSS classes (e.g. "accent", "success") applied to the returned widget control the icon color.
pub fn sport_icon(sport_type: &str, is_indoor: bool) -> gtk::DrawingArea {
    let sport = sport_type.to_lowercase();

    let area = gtk::DrawingArea::builder()
        .content_width(16)
        .content_height(16)
        .valign(gtk::Align::Center)
        .build();

    area.set_draw_func(move |widget, cr, _w, _h| {
        let c = widget.color();
        cr.set_source_rgba(
            c.red() as f64,
            c.green() as f64,
            c.blue() as f64,
            c.alpha() as f64,
        );
        cr.set_line_width(1.5);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);

        match sport.as_str() {
            "ride" | "ebikeride" | "mountainbikeride" | "gravelride" | "handcycle" => {
                if is_indoor {
                    draw_indoor_bike(cr);
                } else {
                    draw_outdoor_bike(cr);
                }
            }
            "virtualride" | "indoorcycling" | "indoorride" | "indoors" => draw_indoor_bike(cr),
            "run" | "virtualrun" | "trailrun" => draw_run(cr),
            "walk" | "hike" | "snowshoe" | "backcountryski" => draw_walk(cr),
            "swim" | "openwater" => draw_swim(cr),
            "weighttraining" | "workout" | "crossfit" | "elliptical" | "stairstepper" => {
                draw_strength(cr)
            }
            _ => draw_generic(cr),
        }
    });

    area
}

// ── Cairo drawing helpers (all use a 16×16 coordinate space) ─────────────────

fn stroke(cr: &cairo::Context) {
    let _ = cr.stroke();
}

fn fill(cr: &cairo::Context) {
    let _ = cr.fill();
}

fn filled_circle(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    fill(cr);
}

fn stroked_circle(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    stroke(cr);
}

fn draw_outdoor_bike(cr: &cairo::Context) {
    // Rear wheel
    stroked_circle(cr, 3.5, 11.5, 3.0);
    // Front wheel
    stroked_circle(cr, 12.5, 11.5, 3.0);
    // Frame: seat tube and chainstay
    cr.move_to(8.0, 5.0);
    cr.line_to(3.5, 11.5);
    stroke(cr);
    cr.move_to(8.0, 5.0);
    cr.line_to(10.0, 11.5);
    stroke(cr);
    // Fork and top tube
    cr.move_to(10.0, 11.5);
    cr.line_to(11.0, 7.0);
    cr.line_to(8.0, 5.0);
    stroke(cr);
    // Handlebar
    cr.move_to(10.5, 6.5);
    cr.line_to(12.5, 6.5);
    stroke(cr);
    // Seat
    cr.move_to(6.5, 5.0);
    cr.line_to(9.0, 5.0);
    stroke(cr);
    // Rider head
    filled_circle(cr, 11.5, 4.5, 1.5);
}

fn draw_indoor_bike(cr: &cairo::Context) {
    // Large drive wheel
    stroked_circle(cr, 8.0, 12.0, 3.5);
    // Trainer stand legs
    cr.move_to(4.5, 14.5);
    cr.line_to(8.0, 12.0);
    cr.line_to(11.5, 14.5);
    stroke(cr);
    // Seat post
    cr.move_to(8.0, 8.5);
    cr.line_to(6.0, 6.5);
    stroke(cr);
    // Seat
    cr.move_to(4.5, 6.0);
    cr.line_to(7.5, 6.0);
    stroke(cr);
    // Handlebar stem
    cr.move_to(8.0, 8.5);
    cr.line_to(10.0, 6.5);
    stroke(cr);
    // Handlebar
    cr.move_to(9.0, 6.0);
    cr.line_to(11.5, 5.5);
    stroke(cr);
    // Rider head
    filled_circle(cr, 5.5, 4.5, 1.5);
}

fn draw_run(cr: &cairo::Context) {
    // Head
    filled_circle(cr, 11.0, 2.5, 1.5);
    // Torso (leaning forward)
    cr.move_to(10.0, 4.0);
    cr.line_to(7.0, 7.5);
    stroke(cr);
    // Forward arm
    cr.move_to(9.0, 5.0);
    cr.line_to(11.5, 7.0);
    stroke(cr);
    // Back arm
    cr.move_to(9.0, 5.5);
    cr.line_to(6.5, 3.5);
    stroke(cr);
    // Forward leg (kick back)
    cr.move_to(7.0, 7.5);
    cr.line_to(9.5, 10.5);
    cr.line_to(8.0, 13.0);
    stroke(cr);
    // Trailing leg (swing forward)
    cr.move_to(7.0, 7.5);
    cr.line_to(4.5, 10.5);
    cr.line_to(6.5, 13.5);
    stroke(cr);
}

fn draw_walk(cr: &cairo::Context) {
    // Head
    filled_circle(cr, 9.0, 2.5, 1.5);
    // Torso (upright)
    cr.move_to(9.0, 4.0);
    cr.line_to(8.0, 8.0);
    stroke(cr);
    // Forward arm
    cr.move_to(8.5, 5.0);
    cr.line_to(11.0, 7.0);
    stroke(cr);
    // Back arm
    cr.move_to(8.5, 5.0);
    cr.line_to(6.0, 6.5);
    stroke(cr);
    // Front leg (step forward)
    cr.move_to(8.0, 8.0);
    cr.line_to(10.0, 11.0);
    cr.line_to(9.0, 14.0);
    stroke(cr);
    // Back leg (push off)
    cr.move_to(8.0, 8.0);
    cr.line_to(6.0, 11.0);
    cr.line_to(7.0, 14.0);
    stroke(cr);
}

fn draw_swim(cr: &cairo::Context) {
    // Head
    filled_circle(cr, 2.5, 6.0, 1.5);
    // Body (horizontal, streamlined)
    cr.move_to(4.0, 6.0);
    cr.line_to(9.0, 7.0);
    stroke(cr);
    // Lead arm (stretched forward)
    cr.move_to(4.5, 5.5);
    cr.line_to(8.5, 4.0);
    stroke(cr);
    // Trailing arm (pull back)
    cr.move_to(6.0, 6.5);
    cr.line_to(7.5, 9.0);
    cr.line_to(10.0, 9.5);
    stroke(cr);
    // Legs / kick
    cr.move_to(9.0, 7.0);
    cr.line_to(12.0, 6.0);
    cr.line_to(14.5, 7.5);
    stroke(cr);
    // Water waves
    cr.move_to(1.0, 11.5);
    cr.curve_to(3.0, 10.0, 5.0, 13.0, 7.5, 11.5);
    cr.curve_to(10.0, 10.0, 12.0, 13.0, 15.0, 11.5);
    stroke(cr);
}

fn draw_strength(cr: &cairo::Context) {
    // Bar
    cr.move_to(4.0, 8.0);
    cr.line_to(12.0, 8.0);
    stroke(cr);
    // Left weight (rounded rect approximated as rect)
    cr.rectangle(1.0, 5.5, 3.0, 5.0);
    fill(cr);
    // Right weight
    cr.rectangle(12.0, 5.5, 3.0, 5.0);
    fill(cr);
    // Left collar
    cr.rectangle(3.5, 6.5, 2.0, 3.0);
    fill(cr);
    // Right collar
    cr.rectangle(10.5, 6.5, 2.0, 3.0);
    fill(cr);
}

fn draw_generic(cr: &cairo::Context) {
    // Head
    filled_circle(cr, 8.0, 3.0, 2.0);
    // Body
    cr.move_to(8.0, 5.0);
    cr.line_to(8.0, 10.0);
    stroke(cr);
    // Arms
    cr.move_to(5.0, 7.0);
    cr.line_to(11.0, 7.0);
    stroke(cr);
    // Left leg
    cr.move_to(8.0, 10.0);
    cr.line_to(5.5, 14.0);
    stroke(cr);
    // Right leg
    cr.move_to(8.0, 10.0);
    cr.line_to(10.5, 14.0);
    stroke(cr);
}
