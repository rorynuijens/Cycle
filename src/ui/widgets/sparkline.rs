//! A small filled line chart for a single day-aligned metric series.
//!
//! Used by the wellness cards on the Fitness page. The series is aligned to one
//! sample per day with `0.0` meaning "no reading that day", so gaps are skipped
//! when plotting but still take up their share of the horizontal axis.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Height of the sparkline in pixels.
const CHART_HEIGHT: i32 = 42;

/// Vertical breathing room so the line and its end dot are not clipped.
const PAD: f64 = 4.0;

/// Project a day-aligned series onto pixel coordinates.
///
/// Days with no reading (`0.0`) are dropped, but the x position still comes
/// from the sample's index in the full series, so a gap shows as a longer
/// segment rather than pulling later readings leftwards.
///
/// Returns an empty vector when there are fewer than two readings, which is
/// not enough to draw a line.
fn plot(values: &[f32], width: f64, height: f64) -> Vec<(f64, f64)> {
    let points: Vec<(usize, f32)> = values
        .iter()
        .enumerate()
        .filter(|&(_, &v)| v > 0.0)
        .map(|(i, &v)| (i, v))
        .collect();
    if points.len() < 2 {
        return Vec::new();
    }

    let min_v = points.iter().map(|&(_, v)| v).fold(f32::MAX, f32::min);
    let max_v = points.iter().map(|&(_, v)| v).fold(f32::MIN, f32::max);
    // A flat series would divide by zero; 1.0 renders it as a level line.
    let range = (max_v - min_v).max(1.0) as f64;
    let last_idx = (values.len() - 1).max(1) as f64;

    points
        .iter()
        .map(|&(i, v)| {
            let x = (i as f64 / last_idx) * width;
            let y = height - PAD - ((v - min_v) as f64 / range) * (height - PAD * 2.0);
            (x, y)
        })
        .collect()
}

/// A filled sparkline backed by a day-aligned series.
pub struct Sparkline {
    area: gtk::DrawingArea,
    values: Rc<RefCell<Vec<f32>>>,
}

impl Sparkline {
    /// Build an empty sparkline.
    ///
    /// The `accent` style class is what makes `widget.color()` resolve to the
    /// GNOME accent colour — libadwaita 1.5 exposes no accent API, so this is
    /// how the chart stays theme-aware without hardcoding a colour.
    pub fn new() -> Self {
        let values: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));

        let area = gtk::DrawingArea::builder()
            .content_height(CHART_HEIGHT)
            .hexpand(true)
            .css_classes(["accent"])
            .build();

        let data = Rc::clone(&values);
        area.set_draw_func(move |widget, cr, width, height| {
            let values = data.borrow();
            let points = plot(&values, width as f64, height as f64);
            let Some(&(first_x, first_y)) = points.first() else {
                return;
            };
            let &(last_x, last_y) = points.last().expect("first() implies last()");

            let accent = widget.color();
            let (r, g, b) = (
                accent.red() as f64,
                accent.green() as f64,
                accent.blue() as f64,
            );
            let h = height as f64;

            // Soft fill between the line and the baseline
            cr.set_source_rgba(r, g, b, 0.10);
            cr.move_to(first_x, h);
            for &(x, y) in &points {
                cr.line_to(x, y);
            }
            cr.line_to(last_x, h);
            cr.close_path();
            cr.fill().ok();

            cr.set_source_rgba(r, g, b, 0.80);
            cr.set_line_width(2.0);
            cr.move_to(first_x, first_y);
            for &(x, y) in &points[1..] {
                cr.line_to(x, y);
            }
            cr.stroke().ok();

            // Mark the most recent reading
            cr.set_source_rgba(r, g, b, 1.0);
            cr.arc(last_x, last_y, 3.5, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        });

        Self { area, values }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Replace the series and redraw.
    pub fn set_values(&self, values: &[f32]) {
        *self.values.borrow_mut() = values.to_vec();
        self.area.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_plot_nothing_for_an_empty_series() {
        assert!(plot(&[], 100.0, 40.0).is_empty());
    }

    #[test]
    fn should_plot_nothing_when_only_one_day_has_a_reading() {
        // One point cannot make a line.
        assert!(plot(&[0.0, 50.0, 0.0], 100.0, 40.0).is_empty());
    }

    #[test]
    fn should_span_the_full_width_when_the_first_and_last_days_have_readings() {
        let points = plot(&[10.0, 20.0, 30.0], 90.0, 40.0);
        assert_eq!(points.len(), 3);
        assert!((points[0].0 - 0.0).abs() < 1e-9);
        assert!((points[2].0 - 90.0).abs() < 1e-9);
    }

    #[test]
    fn should_keep_missing_days_in_their_place_on_the_axis() {
        // Readings on day 0 and day 3 of a 4-day series: the second point
        // belongs at the far right, not halfway along.
        let points = plot(&[10.0, 0.0, 0.0, 20.0], 60.0, 40.0);
        assert_eq!(points.len(), 2);
        assert!((points[1].0 - 60.0).abs() < 1e-9);
    }

    #[test]
    fn should_put_the_highest_reading_at_the_top() {
        let points = plot(&[10.0, 90.0], 100.0, 40.0);
        let (_, low_y) = points[0];
        let (_, high_y) = points[1];
        // Cairo y grows downwards, so the larger value sits at the smaller y.
        assert!(high_y < low_y, "peak should draw above the trough");
        assert!(
            (high_y - PAD).abs() < 1e-9,
            "peak sits one pad below the top"
        );
    }

    #[test]
    fn should_draw_a_flat_series_without_dividing_by_zero() {
        let points = plot(&[25.0, 25.0, 25.0], 100.0, 40.0);
        assert_eq!(points.len(), 3);
        assert!(points.iter().all(|&(_, y)| y.is_finite()));
        // All equal values share one y.
        assert!((points[0].1 - points[2].1).abs() < 1e-9);
    }

    #[test]
    fn should_keep_every_point_inside_the_chart() {
        let points = plot(&[5.0, 100.0, 3.0, 62.0], 120.0, 42.0);
        assert!(points.iter().all(|&(_, y)| (PAD..=42.0 - PAD).contains(&y)));
    }
}
