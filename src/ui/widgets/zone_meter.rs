use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::data::athlete::{power_zone_index, ZONE_COLORS};

/// Zone labels indexed by `power_zone_index` — used for the accessible label
/// and tooltip so the meter is readable without colour vision.
/// Shared with the summary page's zone legend.
pub const ZONE_LABELS: [&str; 7] = [
    "Z1 Recovery",
    "Z2 Endurance",
    "Z3 Tempo",
    "Z4 Threshold",
    "Z5 VO₂Max",
    "Z6 Anaerobic",
    "Z7 Neuromuscular",
];

/// Zone band boundaries as % of FTP. The ribbon spans 0–170 % FTP; Z7 is
/// open-ended in the model, so it gets the 150–170 % band and the marker
/// clamps to the right edge beyond that.
const ZONE_BOUNDS_PCT: [f64; 8] = [0.0, 55.0, 75.0, 90.0, 105.0, 120.0, 150.0, 170.0];

/// Custom Cairo-drawn power-zone ribbon.
///
/// Renders all seven Coggan zones as proportional colour bands (via
/// `ZONE_COLORS` — the only sanctioned expressive colour, see CLAUDE.md §1.6).
/// The band containing the current power is drawn at full saturation while the
/// rest stay dimmed, and a foreground-coloured marker rides the live wattage.
pub struct ZoneMeter {
    drawing_area: gtk::DrawingArea,
    power_watts: Rc<Cell<Option<u32>>>,
    ftp_watts: Rc<Cell<u32>>,
    /// Last zone announced via tooltip/accessible label — avoids re-setting
    /// the property every second when the zone hasn't changed.
    last_zone: Cell<Option<usize>>,
}

impl ZoneMeter {
    pub fn new(ftp_watts: u32) -> Self {
        let drawing_area = gtk::DrawingArea::builder()
            .content_height(24)
            .hexpand(true)
            .build();

        let power = Rc::new(Cell::new(None::<u32>));
        let ftp = Rc::new(Cell::new(ftp_watts.max(1)));

        let power_draw = Rc::clone(&power);
        let ftp_draw = Rc::clone(&ftp);
        drawing_area.set_draw_func(move |widget, cr, width, height| {
            Self::draw(widget, cr, width, height, power_draw.get(), ftp_draw.get());
        });

        let meter = Self {
            drawing_area,
            power_watts: power,
            ftp_watts: ftp,
            last_zone: Cell::new(None),
        };
        meter.announce_zone();
        meter
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.drawing_area
    }

    /// Update the live power reading (None = no data / show all bands dimmed).
    pub fn set_power(&self, watts: Option<u32>) {
        self.power_watts.set(watts);
        self.announce_zone();
        self.drawing_area.queue_draw();
    }

    /// Update the FTP the zone scale is derived from.
    pub fn set_ftp(&self, ftp_watts: u32) {
        let clamped = ftp_watts.max(1);
        if self.ftp_watts.get() != clamped {
            self.ftp_watts.set(clamped);
            self.drawing_area.queue_draw();
        }
    }

    /// Keep the tooltip and accessible label in sync with the current zone so
    /// the meter is meaningful to screen readers and without colour vision.
    fn announce_zone(&self) {
        let zone = self
            .power_watts
            .get()
            .map(|w| power_zone_index(w, self.ftp_watts.get()));
        if zone == self.last_zone.get() {
            return;
        }
        self.last_zone.set(zone);
        let text = match zone {
            Some(z) => format!("Power zone: {}", ZONE_LABELS[z]),
            None => String::from("Power zone meter — waiting for power data"),
        };
        self.drawing_area.set_tooltip_text(Some(&text));
        self.drawing_area
            .update_property(&[gtk::accessible::Property::Label(&text)]);
    }

    fn draw(
        widget: &gtk::DrawingArea,
        cr: &gtk::cairo::Context,
        width: i32,
        height: i32,
        power_watts: Option<u32>,
        ftp_watts: u32,
    ) {
        let w = width as f64;
        let h = height as f64;
        let scale_max = ZONE_BOUNDS_PCT[7]; // ribbon spans 0–170 % FTP

        // Band geometry: a slim pill vertically centred; the marker overhangs it.
        let band_h = h / 2.0;
        let band_y = (h - band_h) / 2.0;
        let radius = band_h / 2.0;

        let current_zone = power_watts.map(|p| power_zone_index(p, ftp_watts));

        // Clip to a pill so the outer bands get rounded ends without per-band arcs.
        cr.save().ok();
        cr.new_path();
        cr.arc(
            radius,
            band_y + radius,
            radius,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI + std::f64::consts::FRAC_PI_2,
        );
        cr.arc(
            w - radius,
            band_y + radius,
            radius,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        cr.clip();

        for zone in 0..7 {
            let x0 = ZONE_BOUNDS_PCT[zone] / scale_max * w;
            let x1 = ZONE_BOUNDS_PCT[zone + 1] / scale_max * w;
            let (r, g, b) = ZONE_COLORS[zone];
            let alpha = if current_zone == Some(zone) { 1.0 } else { 0.3 };
            cr.set_source_rgba(r, g, b, alpha);
            // 1 px gap between bands encodes the zone boundary itself.
            cr.rectangle(x0, band_y, (x1 - x0 - 1.0).max(0.0), band_h);
            cr.fill().ok();
        }
        cr.restore().ok();

        // Marker: theme foreground colour so it reads in both light and dark.
        if let Some(watts) = power_watts {
            let pct = watts as f64 / ftp_watts as f64 * 100.0;
            let x = (pct / scale_max * w).clamp(1.0, w - 1.0);
            let fg = widget.color();
            cr.set_source_rgba(fg.red().into(), fg.green().into(), fg.blue().into(), 1.0);
            cr.set_line_width(2.0);
            cr.move_to(x, 0.0);
            cr.line_to(x, h);
            cr.stroke().ok();
        }
    }
}
