use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::AthleteProfile;
use crate::data::workout::Workout;

/// Custom Cairo-drawn workout profile graph.
///
/// Renders power targets as zone-coloured bars, an FTP reference line,
/// a live power trace, and a playhead.
pub struct WorkoutGraph {
    drawing_area: gtk::DrawingArea,
    workout: Rc<RefCell<Workout>>,
    #[allow(dead_code)]
    athlete: Rc<AthleteProfile>,
    playhead_secs: Rc<RefCell<u32>>,
    power_trace: Rc<RefCell<Vec<Option<u32>>>>,
}

impl WorkoutGraph {
    pub fn new(workout: &Workout, athlete: &AthleteProfile) -> Self {
        let drawing_area = gtk::DrawingArea::builder()
            .content_width(600)
            .content_height(120)
            .hexpand(true)
            .vexpand(false)
            .build();

        let workout_rc = Rc::new(RefCell::new(workout.clone()));
        let athlete_rc = Rc::new(athlete.clone());
        let playhead = Rc::new(RefCell::new(0u32));
        let power_trace = Rc::new(RefCell::new(Vec::<Option<u32>>::new()));

        let wo = Rc::clone(&workout_rc);
        let at = Rc::clone(&athlete_rc);
        let ph = Rc::clone(&playhead);
        let tr = Rc::clone(&power_trace);

        drawing_area.set_draw_func(move |widget, cr, width, height| {
            Self::draw(
                widget,
                cr,
                width,
                height,
                &wo.borrow(),
                &at,
                *ph.borrow(),
                &tr.borrow(),
            );
        });

        Self {
            drawing_area,
            workout: workout_rc,
            athlete: athlete_rc,
            playhead_secs: playhead,
            power_trace,
        }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.drawing_area
    }

    pub fn set_playhead(&self, elapsed_secs: u32) {
        *self.playhead_secs.borrow_mut() = elapsed_secs;
        self.drawing_area.queue_draw();
    }

    pub fn push_power(&self, watts: Option<u32>) {
        self.power_trace.borrow_mut().push(watts);
        self.drawing_area.queue_draw();
    }

    /// Replace the displayed workout and reset the trace and playhead.
    pub fn set_workout(&self, workout: &Workout) {
        *self.workout.borrow_mut() = workout.clone();
        self.power_trace.borrow_mut().clear();
        *self.playhead_secs.borrow_mut() = 0;
        self.drawing_area.queue_draw();
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        widget: &gtk::DrawingArea,
        cr: &gtk::cairo::Context,
        width: i32,
        height: i32,
        workout: &Workout,
        athlete: &AthleteProfile,
        playhead_secs: u32,
        power_trace: &[Option<u32>],
    ) {
        let w = width as f64;
        let h = height as f64;
        // Theme foreground colour — keeps the overlay lines legible in both
        // light and dark themes without hardcoding a colour (CLAUDE.md §1.6).
        let fg = widget.color();
        let (fg_r, fg_g, fg_b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
        let total_secs = workout.duration_secs as f64;
        // Scale: 125% FTP fills the graph vertically
        let max_watts = athlete.ftp_watts as f64 * 1.25;

        // ── Segment bars ─────────────────────────────────────────────────────
        let mut elapsed = 0u32;
        for segment in &workout.segments {
            let x_start = (elapsed as f64 / total_secs) * w;
            let x_width = (segment.duration_secs as f64 / total_secs) * w;

            let y_low = h
                - (segment.power_low_pct as f64 / 100.0 * athlete.ftp_watts as f64 / max_watts) * h;
            let y_high = h
                - (segment.power_high_pct as f64 / 100.0 * athlete.ftp_watts as f64 / max_watts)
                    * h;

            let mid_pct = (segment.power_low_pct + segment.power_high_pct) / 2.0;
            let zone =
                athlete.power_zone((mid_pct as f64 / 100.0 * athlete.ftp_watts as f64) as u32);
            let (r, g, b) = zone.rgb();

            let past = (elapsed + segment.duration_secs) as f64 / total_secs
                < playhead_secs as f64 / total_secs;
            let alpha = if past { 0.35 } else { 0.65 };

            cr.set_source_rgba(r, g, b, alpha);
            cr.move_to(x_start, h);
            cr.line_to(x_start, y_low);
            if segment.is_ramp() {
                cr.line_to(x_start + x_width, y_high);
            } else {
                cr.line_to(x_start, y_high);
                cr.line_to(x_start + x_width, y_high);
            }
            cr.line_to(x_start + x_width, h);
            cr.close_path();
            cr.fill().ok();

            elapsed += segment.duration_secs;
        }

        // ── FTP reference line (dashed) ──────────────────────────────────────
        let ftp_y = h - (athlete.ftp_watts as f64 / max_watts) * h;
        cr.set_source_rgba(fg_r, fg_g, fg_b, 0.25);
        cr.set_line_width(1.0);
        cr.set_dash(&[4.0, 4.0], 0.0);
        cr.move_to(0.0, ftp_y);
        cr.line_to(w, ftp_y);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);

        // ── Live power trace (fill + stroke) ────────────────────────────────
        if !power_trace.is_empty() {
            // Semi-transparent fill under the trace
            cr.set_source_rgba(fg_r, fg_g, fg_b, 0.10);
            let mut last_x = 0.0f64;
            let mut started = false;
            for (i, &watts) in power_trace.iter().enumerate() {
                if let Some(w_val) = watts {
                    let x = (i as f64 / total_secs) * w;
                    let y = h - (w_val as f64 / max_watts) * h;
                    if !started {
                        cr.move_to(x, h);
                        cr.line_to(x, y);
                        started = true;
                    } else {
                        cr.line_to(x, y);
                    }
                    last_x = x;
                }
            }
            if started {
                cr.line_to(last_x, h);
                cr.close_path();
                cr.fill().ok();
            }

            // Solid stroke over the fill
            cr.set_source_rgba(fg_r, fg_g, fg_b, 0.85);
            cr.set_line_width(1.5);
            let mut first = true;
            for (i, &watts) in power_trace.iter().enumerate() {
                if let Some(w_val) = watts {
                    let x = (i as f64 / total_secs) * w;
                    let y = h - (w_val as f64 / max_watts) * h;
                    if first {
                        cr.move_to(x, y);
                        first = false;
                    } else {
                        cr.line_to(x, y);
                    }
                }
            }
            cr.stroke().ok();
        }

        // ── Playhead ─────────────────────────────────────────────────────────
        if playhead_secs > 0 {
            let ph_x = (playhead_secs as f64 / total_secs) * w;
            cr.set_source_rgba(fg_r, fg_g, fg_b, 0.9);
            cr.set_line_width(2.0);
            cr.move_to(ph_x, 0.0);
            cr.line_to(ph_x, h);
            cr.stroke().ok();
        }
    }
}
