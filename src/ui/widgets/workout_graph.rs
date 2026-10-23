use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::{power_zone_index, ZONE_COLORS};
use crate::data::workout::Workout;
use crate::training::engine::{EngineSnapshot, EngineState};

/// Custom Cairo-drawn workout profile graph.
///
/// Renders power targets as zone-coloured bars, an FTP reference line,
/// a live power trace, and a playhead.
pub struct WorkoutGraph {
    drawing_area: gtk::DrawingArea,
    workout: Rc<RefCell<Workout>>,
    /// FTP the vertical scale and zone colours are derived from. Held as the
    /// bare number, not a profile: taking a whole `AthleteProfile` here invited
    /// callers to synthesise one from `default()` just to carry an FTP, which
    /// would quietly drop any field added to the profile later.
    ftp_watts: Rc<RefCell<u32>>,
    playhead_secs: Rc<RefCell<u32>>,
    power_trace: Rc<RefCell<Vec<Option<u32>>>>,
}

impl WorkoutGraph {
    pub fn new(workout: &Workout, ftp_watts: u32) -> Self {
        let drawing_area = gtk::DrawingArea::builder()
            .content_width(600)
            .content_height(120)
            .hexpand(true)
            .vexpand(false)
            .build();

        let workout_rc = Rc::new(RefCell::new(workout.clone()));
        // Guard the divisions in `draw` — an FTP of 0 would otherwise scale
        // every bar to NaN and blank the graph.
        let ftp_rc = Rc::new(RefCell::new(ftp_watts.max(1)));
        let playhead = Rc::new(RefCell::new(0u32));
        let power_trace = Rc::new(RefCell::new(Vec::<Option<u32>>::new()));

        let wo = Rc::clone(&workout_rc);
        let ftp = Rc::clone(&ftp_rc);
        let ph = Rc::clone(&playhead);
        let tr = Rc::clone(&power_trace);

        drawing_area.set_draw_func(move |widget, cr, width, height| {
            Self::draw(
                widget,
                cr,
                width,
                height,
                &wo.borrow(),
                *ftp.borrow(),
                *ph.borrow(),
                &tr.borrow(),
            );
        });

        Self {
            drawing_area,
            workout: workout_rc,
            ftp_watts: ftp_rc,
            playhead_secs: playhead,
            power_trace,
        }
    }

    /// Rescale the graph to a new FTP — call after the rider edits their
    /// profile so an already-built graph does not keep the old scale.
    ///
    /// A no-op when the value is unchanged, so per-tick callers pay nothing.
    pub fn set_ftp(&self, ftp_watts: u32) {
        let ftp = ftp_watts.max(1);
        if *self.ftp_watts.borrow() == ftp {
            return;
        }
        *self.ftp_watts.borrow_mut() = ftp;
        self.drawing_area.queue_draw();
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.drawing_area
    }

    /// Draw one tick of a live ride: move the playhead, and plot the power at
    /// the second the ride is on — never simply after the last sample. Ticks
    /// the ride does not record (the pre-start wait, a pause) are not plotted.
    pub fn record_live(&self, snap: &EngineSnapshot) {
        *self.playhead_secs.borrow_mut() = snap.elapsed_secs;
        if let Some((second, watts)) = live_sample(snap) {
            place_sample(&mut self.power_trace.borrow_mut(), second, watts);
        }
        self.drawing_area.queue_draw();
    }

    /// Replace the whole power trace at once — used when displaying a
    /// finished session rather than a live one.
    pub fn set_trace(&self, trace: Vec<Option<u32>>) {
        *self.power_trace.borrow_mut() = trace;
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
        ftp_watts: u32,
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
        let ftp = ftp_watts as f64;
        // Scale: 125% FTP fills the graph vertically
        let max_watts = ftp * 1.25;

        // ── Segment bars ─────────────────────────────────────────────────────
        let mut elapsed = 0u32;
        for segment in &workout.segments {
            let x_start = (elapsed as f64 / total_secs) * w;
            let x_width = (segment.duration_secs as f64 / total_secs) * w;

            let y_low = h - (segment.power_low_pct as f64 / 100.0 * ftp / max_watts) * h;
            let y_high = h - (segment.power_high_pct as f64 / 100.0 * ftp / max_watts) * h;

            let mid_pct = (segment.power_low_pct + segment.power_high_pct) / 2.0;
            let mid_watts = (mid_pct as f64 / 100.0 * ftp) as u32;
            let (r, g, b) = ZONE_COLORS[power_zone_index(mid_watts, ftp_watts)];

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
        let ftp_y = h - (ftp / max_watts) * h;
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

/// The trace slot a live tick fills, and its power, or `None` for a tick the
/// ride does not record. Mirrors what `WorkoutEngine::tick` saves, so the live
/// chart and the finished ride's chart are drawn from the same seconds.
fn live_sample(snap: &EngineSnapshot) -> Option<(usize, Option<u32>)> {
    (snap.state == EngineState::Running)
        .then_some((snap.elapsed_secs as usize, snap.readings.power_watts))
}

/// Put `watts` at `second`, growing the trace with gaps if it is short.
fn place_sample(trace: &mut Vec<Option<u32>>, second: usize, watts: Option<u32>) {
    if trace.len() <= second {
        trace.resize(second + 1, None);
    }
    trace[second] = watts;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::LiveReadings;

    fn snap(state: EngineState, elapsed_secs: u32, watts: u32) -> EngineSnapshot {
        EngineSnapshot {
            state,
            elapsed_secs,
            remaining_secs: 0,
            segment_index: 0,
            segment_elapsed_secs: 0,
            segment_remaining_secs: 0,
            target_power_watts: 0,
            intensity_pct: 100,
            readings: LiveReadings {
                power_watts: Some(watts),
                ..Default::default()
            },
        }
    }

    /// What the live chart holds after a run of ticks.
    fn trace_after(ticks: &[EngineSnapshot]) -> Vec<Option<u32>> {
        let mut trace = Vec::new();
        for t in ticks {
            if let Some((second, watts)) = live_sample(t) {
                place_sample(&mut trace, second, watts);
            }
        }
        trace
    }

    #[test]
    fn should_start_the_trace_at_zero_however_long_the_wait_to_start() {
        // Ride #20 (2026-10-23): the page opened 50 s before the rider began
        // pedalling, and every interval was drawn 50 s late.
        let mut ticks: Vec<_> = (0..50).map(|_| snap(EngineState::Idle, 0, 0)).collect();
        ticks.extend((0..10).map(|s| snap(EngineState::Running, s, 200 + s)));
        let trace = trace_after(&ticks);
        assert_eq!(trace.len(), 10);
        assert_eq!(trace[0], Some(200));
        assert_eq!(trace[9], Some(209));
    }

    #[test]
    fn should_not_shift_the_trace_after_a_pause() {
        let mut ticks: Vec<_> = (0..5).map(|s| snap(EngineState::Running, s, 100)).collect();
        ticks.extend((0..30).map(|_| snap(EngineState::Paused, 5, 0)));
        ticks.push(snap(EngineState::Running, 5, 250));
        let trace = trace_after(&ticks);
        assert_eq!(trace.len(), 6);
        assert_eq!(trace[5], Some(250));
    }

    #[test]
    fn should_not_plot_a_tick_the_ride_does_not_record() {
        for state in [
            EngineState::Idle,
            EngineState::Paused,
            EngineState::Completed,
        ] {
            assert_eq!(live_sample(&snap(state, 12, 180)), None, "{state:?}");
        }
        assert_eq!(
            live_sample(&snap(EngineState::Running, 12, 180)),
            Some((12, Some(180)))
        );
    }

    #[test]
    fn should_leave_a_gap_for_seconds_the_chart_never_saw() {
        let mut trace = vec![Some(100)];
        place_sample(&mut trace, 3, Some(140));
        assert_eq!(trace, vec![Some(100), None, None, Some(140)]);
    }

    #[test]
    fn should_keep_drawing_into_a_trace_filled_from_the_ride_so_far() {
        // The overlay opens mid-ride with the whole workout's length already
        // laid out; a live second must land inside it, not after its end.
        let mut trace = vec![None; 100];
        place_sample(&mut trace, 40, Some(230));
        assert_eq!(trace.len(), 100);
        assert_eq!(trace[40], Some(230));
    }

    #[test]
    fn should_record_a_dropout_as_a_gap_not_a_stale_value() {
        let mut trace = vec![Some(100), Some(110)];
        place_sample(&mut trace, 1, None);
        assert_eq!(trace, vec![Some(100), None]);
    }
}
