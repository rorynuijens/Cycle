//! "Bests" — the rider's peak power and peak pace, all time against last month.
//!
//! Both curves share the same chrome (chart, duration axis, per-point values,
//! legend) and differ only in how they scale and colour their points.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::{power_zone_index, AthleteProfile, ZONE_COLORS};
use crate::training::analytics::{format_pace_display, PACE_LABELS};

/// Display labels for [`CURVE_DURATIONS`], in the same order.
const CURVE_LABELS: [&str; 10] = [
    "5s", "10s", "30s", "1m", "2m", "5m", "10m", "20m", "30m", "60m",
];

/// Breathing room at the top and bottom of a curve chart.
const PAD: f64 = 4.0;

/// The smallest pace spread a chart will plot, in seconds per kilometre.
///
/// Without a floor, a rider whose 5 km and 10 km paces are seconds apart would
/// get a chart that magnifies that gap into a cliff.
const MIN_PACE_SPREAD: f64 = 30.0;

/// Vertical position of a power value: the biggest number sits at the top,
/// a missing value drops off the bottom of the chart.
fn power_y(watts: u32, max_watts: u32, height: f64) -> f64 {
    if watts == 0 || max_watts == 0 {
        return height;
    }
    let usable = height - PAD;
    usable - (watts as f64 / max_watts as f64) * (usable - PAD)
}

/// The pace spread a chart plots, across both the all-time and recent curves.
///
/// Returns `None` when nothing has been run, which hides the section.
fn pace_range(curve: &[(u32, u32)]) -> Option<(u32, u32)> {
    let paces: Vec<u32> = curve
        .iter()
        .flat_map(|&(all_time, recent)| [all_time, recent])
        .filter(|&p| p > 0)
        .collect();
    let min = paces.iter().copied().min()?;
    let max = paces.iter().copied().max()?;
    Some((min, max))
}

/// Vertical position of a pace. Pace is seconds per kilometre, so *lower* is
/// faster and belongs at the top of the chart.
fn pace_y(pace: u32, min_pace: u32, max_pace: u32, height: f64) -> f64 {
    let spread = (max_pace as f64 - min_pace as f64).max(MIN_PACE_SPREAD);
    let ratio = ((pace as f64 - min_pace as f64) / spread).clamp(0.0, 1.0);
    PAD + ratio * (height - PAD - PAD)
}

/// Evenly spaced x positions across the chart width.
fn x_at(index: usize, count: usize, width: f64) -> f64 {
    (index as f64 / (count - 1).max(1) as f64) * width
}

/// Shared data handle for a curve: `(all-time best, best in the last 30 days)`
/// per duration or distance.
type CurveData = Rc<RefCell<Vec<(u32, u32)>>>;

/// One curve chart with its axis labels, per-point values, and legend.
struct CurveSection {
    root: gtk::Box,
    chart: gtk::DrawingArea,
    value_labels: Vec<gtk::Label>,
    data: CurveData,
}

impl CurveSection {
    /// `build_chart` is handed the shared data and returns the drawing area, so
    /// each curve keeps its own scaling and colouring.
    fn new(
        heading: &str,
        tooltip: &str,
        x_labels: &[&str],
        legend: &str,
        build_chart: impl FnOnce(&CurveData) -> gtk::DrawingArea,
    ) -> Self {
        let data: CurveData = Rc::new(RefCell::new(vec![(0, 0); x_labels.len()]));
        let chart = build_chart(&data);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        root.append(
            &gtk::Label::builder()
                .label(heading)
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(tooltip)
                .build(),
        );
        root.append(&chart);

        let x_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for label in x_labels {
            x_row.append(
                &gtk::Label::builder()
                    .label(*label)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }
        root.append(&x_row);

        let value_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        let mut value_labels = Vec::with_capacity(x_labels.len());
        for _ in x_labels {
            let label = gtk::Label::builder()
                .label("—")
                .css_classes(["caption", "numeric"])
                .halign(gtk::Align::Center)
                .build();
            value_row.append(&label);
            value_labels.push(label);
        }
        root.append(&value_row);

        root.append(
            &gtk::Label::builder()
                .label(legend)
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Start)
                .build(),
        );

        Self {
            root,
            chart,
            value_labels,
            data,
        }
    }

    /// Replace the curve. Returns whether there was anything to plot, and hides
    /// itself when there was not.
    fn set_curve(&self, curve: Vec<(u32, u32)>, format: impl Fn(u32) -> String) -> bool {
        let has_data = curve.iter().any(|&(all_time, _)| all_time > 0);
        self.root.set_visible(has_data);
        if !has_data {
            return false;
        }

        for (label, &(all_time, _)) in self.value_labels.iter().zip(curve.iter()) {
            if all_time > 0 {
                label.set_label(&format(all_time));
            } else {
                label.set_label("—");
            }
        }
        *self.data.borrow_mut() = curve;
        self.chart.queue_draw();
        true
    }
}

/// The Bests section: peak power over peak pace.
pub struct BestsSection {
    root: gtk::Box,
    power: CurveSection,
    pace: CurveSection,
}

impl BestsSection {
    pub fn new(athlete: Rc<RefCell<AthleteProfile>>) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        root.append(
            &gtk::Label::builder()
                .label("Bests")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        let power = CurveSection::new(
            "Peak power",
            "Best average power for each duration across all recorded sessions, \
             coloured by the power zone it falls in at your current FTP",
            &CURVE_LABELS,
            "Dots coloured by power zone · dimmed line = last 30 days",
            |data| Self::build_power_chart(data, athlete),
        );
        root.append(&power.root);

        let pace = CurveSection::new(
            "Running pace",
            "Best pace for each distance across all synced running activities",
            &PACE_LABELS,
            "Solid = all time · dimmed = last 30 days",
            Self::build_pace_chart,
        );
        root.append(&pace.root);

        Self { root, power, pace }
    }

    /// The power curve: a quiet line carrying dots coloured by the power zone
    /// each best falls in — the sprint end glows anaerobic red, the hour end
    /// sits at threshold. Zone RGB is the app's only sanctioned expressive
    /// colour (CLAUDE.md §1.6).
    fn build_power_chart(
        data: &CurveData,
        athlete: Rc<RefCell<AthleteProfile>>,
    ) -> gtk::DrawingArea {
        let chart = gtk::DrawingArea::builder()
            .content_height(130)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        chart.update_property(&[gtk::accessible::Property::Label(
            "Power curve chart: best mean maximal power for durations from 1 second to 60 minutes",
        )]);

        let data = Rc::clone(data);
        chart.set_draw_func(move |widget, cr, width, height| {
            let curve = data.borrow();
            let max_watts = curve
                .iter()
                .map(|&(all_time, _)| all_time)
                .max()
                .unwrap_or(0);
            if max_watts == 0 {
                return;
            }
            let ftp = athlete.borrow().ftp_watts;
            let fg = widget.color();
            let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let (w, h) = (width as f64, height as f64);
            let n = curve.len();

            // Recent curve first, so the all-time zone dots sit on top.
            let recent: Vec<(f64, f64)> = curve
                .iter()
                .enumerate()
                .filter(|&(_, &(_, recent))| recent > 0)
                .map(|(i, &(_, recent))| (x_at(i, n, w), power_y(recent, max_watts, h)))
                .collect();
            if recent.len() >= 2 {
                cr.set_source_rgba(r, g, b, 0.40);
                cr.set_line_width(1.5);
                cr.move_to(recent[0].0, recent[0].1);
                for &(x, y) in &recent[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(x, y) in &recent {
                    cr.arc(x, y, 2.5, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            }

            let all_time: Vec<(u32, f64, f64)> = curve
                .iter()
                .enumerate()
                .filter(|&(_, &(watts, _))| watts > 0)
                .map(|(i, &(watts, _))| (watts, x_at(i, n, w), power_y(watts, max_watts, h)))
                .collect();
            if all_time.len() >= 2 {
                cr.set_source_rgba(r, g, b, 0.30);
                cr.set_line_width(1.5);
                cr.move_to(all_time[0].1, all_time[0].2);
                for &(_, x, y) in &all_time[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(watts, x, y) in &all_time {
                    let (zr, zg, zb) = ZONE_COLORS[power_zone_index(watts, ftp)];
                    cr.set_source_rgba(zr, zg, zb, 1.0);
                    cr.arc(x, y, 4.0, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            }
        });

        chart
    }

    /// The pace curve: solid line for all-time bests, dimmed for the last 30 days.
    fn build_pace_chart(data: &CurveData) -> gtk::DrawingArea {
        let chart = gtk::DrawingArea::builder()
            .content_height(130)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        chart.update_property(&[gtk::accessible::Property::Label(
            "Pace curve chart: best times at standard running distances (400 m to marathon)",
        )]);

        let data = Rc::clone(data);
        chart.set_draw_func(move |widget, cr, width, height| {
            let curve = data.borrow();
            let Some((min_pace, max_pace)) = pace_range(&curve) else {
                return;
            };
            let fg = widget.color();
            let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let (w, h) = (width as f64, height as f64);
            let n = curve.len();

            let draw = |points: &[(f64, f64)], alpha: f64, radius: f64| {
                if points.len() < 2 {
                    return;
                }
                cr.set_source_rgba(r, g, b, alpha);
                cr.set_line_width(2.0);
                cr.move_to(points[0].0, points[0].1);
                for &(x, y) in &points[1..] {
                    cr.line_to(x, y);
                }
                cr.stroke().ok();
                for &(x, y) in points {
                    cr.arc(x, y, radius, 0.0, std::f64::consts::TAU);
                    cr.fill().ok();
                }
            };

            let recent: Vec<(f64, f64)> = curve
                .iter()
                .enumerate()
                .filter(|&(_, &(_, recent))| recent > 0)
                .map(|(i, &(_, recent))| (x_at(i, n, w), pace_y(recent, min_pace, max_pace, h)))
                .collect();
            draw(&recent, 0.40, 2.5);

            let all_time: Vec<(f64, f64)> = curve
                .iter()
                .enumerate()
                .filter(|&(_, &(all_time, _))| all_time > 0)
                .map(|(i, &(all_time, _))| (x_at(i, n, w), pace_y(all_time, min_pace, max_pace, h)))
                .collect();
            draw(&all_time, 0.85, 3.5);
        });

        chart
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Update both curves. The section hides when neither has anything to show.
    pub fn set_curves(&self, power_curve: Vec<(u32, u32)>, pace_curve: Vec<(u32, u32)>) {
        let has_power = self
            .power
            .set_curve(power_curve, |watts| format!("{watts}W"));
        let has_pace = self.pace.set_curve(pace_curve, |pace| {
            format!("{}/km", format_pace_display(pace))
        });
        self.root.set_visible(has_power || has_pace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::training::analytics::{CURVE_DURATIONS, PACE_DISTANCES};

    #[test]
    fn should_label_every_duration_and_distance_on_the_axis() {
        assert_eq!(CURVE_LABELS.len(), CURVE_DURATIONS.len());
        assert_eq!(PACE_LABELS.len(), PACE_DISTANCES.len());
    }

    #[test]
    fn should_put_the_best_power_at_the_top() {
        let y = power_y(500, 500, 130.0);
        assert!((y - PAD).abs() < 1e-9, "peak sits one pad below the top");
    }

    #[test]
    fn should_drop_a_missing_power_off_the_bottom() {
        // A duration never ridden must not plot as "zero watts at the axis".
        assert_eq!(power_y(0, 500, 130.0), 130.0);
    }

    #[test]
    fn should_not_divide_by_zero_without_any_power_data() {
        assert_eq!(power_y(200, 0, 130.0), 130.0);
    }

    #[test]
    fn should_place_half_the_peak_power_around_the_middle() {
        let peak = power_y(500, 500, 130.0);
        let half = power_y(250, 500, 130.0);
        let floor = 130.0 - PAD;
        assert!((half - (peak + floor) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn should_find_no_pace_range_before_the_first_run() {
        assert_eq!(pace_range(&[(0, 0), (0, 0)]), None);
        assert_eq!(pace_range(&[]), None);
    }

    #[test]
    fn should_span_both_curves_when_ranging_pace() {
        // Recent bests can be faster than the all-time entry for another
        // distance, so both series must be considered.
        assert_eq!(pace_range(&[(300, 250), (0, 400)]), Some((250, 400)));
    }

    #[test]
    fn should_put_the_fastest_pace_at_the_top() {
        // Lower seconds per km is faster, and Cairo y grows downwards.
        let fast = pace_y(240, 240, 400, 130.0);
        let slow = pace_y(400, 240, 400, 130.0);
        assert!(fast < slow, "the faster pace should draw higher");
        assert!((fast - PAD).abs() < 1e-9);
    }

    #[test]
    fn should_not_magnify_a_narrow_pace_spread() {
        // Paces 5 s/km apart must not stretch across the whole chart.
        let fast = pace_y(300, 300, 305, 130.0);
        let slow = pace_y(305, 300, 305, 130.0);
        let spanned = slow - fast;
        assert!(spanned < 130.0 * 0.25, "spanned {spanned} of the chart");
    }

    #[test]
    fn should_keep_every_pace_inside_the_chart() {
        for pace in [240u32, 300, 400, 600] {
            let y = pace_y(pace, 240, 600, 130.0);
            assert!((PAD..=130.0 - PAD).contains(&y), "pace {pace} gave y {y}");
        }
    }

    #[test]
    fn should_spread_points_evenly_from_edge_to_edge() {
        assert_eq!(x_at(0, 5, 100.0), 0.0);
        assert_eq!(x_at(4, 5, 100.0), 100.0);
        assert_eq!(x_at(2, 5, 100.0), 50.0);
    }

    #[test]
    fn should_not_divide_by_zero_for_a_single_point() {
        assert_eq!(x_at(0, 1, 100.0), 0.0);
    }
}
