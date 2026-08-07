//! The page's headline: today's form, and the 90 days of training that produced it.
//!
//! TSB is the one number that says what the rider can absorb today. CTL and ATL
//! are the supporting pair, and the performance-management chart below shows how
//! they got here.

use adw::prelude::*;
use chrono::Datelike;
use std::cell::RefCell;
use std::rc::Rc;

use crate::training::fitness::{tsb_status_text, PmcPoint, TsbBand};

/// Space reserved at the bottom of the chart for x-axis date labels.
const AXIS_PAD_BOTTOM: f64 = 22.0;

/// Space reserved above the chart so the highest point is not clipped.
const AXIS_PAD_TOP: f64 = 6.0;

/// The vertical range the chart plots, across all three series.
///
/// Anchored to include zero (the TSB fill is drawn against it) and to show at
/// least 10 units, so an athlete with barely any load does not get a chart
/// whose noise fills the whole height.
fn plot_range(points: &[PmcPoint]) -> (f64, f64) {
    let values = points.iter().flat_map(|p| [p.ctl, p.atl, p.tsb]);
    let (mut min, mut max) = (0.0f64, 10.0f64);
    for v in values {
        min = min.min(v);
        max = max.max(v);
    }
    (min, max)
}

/// The form hero: TSB headline, CTL/ATL pair, and the 90-day PMC chart.
pub struct FormHero {
    root: gtk::Box,
    tsb_label: gtk::Label,
    form_phrase: gtk::Label,
    ctl_atl_pair: gtk::Label,
    icu_indicator: gtk::Label,
    pmc_section: gtk::Box,
    pmc_chart: gtk::DrawingArea,
    pmc_data: Rc<RefCell<Vec<PmcPoint>>>,
}

impl FormHero {
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        root.append(
            &gtk::Label::builder()
                .label("Form")
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(
                    "Form (TSB) is fitness (CTL) minus fatigue (ATL) — exponential moving \
                     averages of your daily training stress. Positive means fresh, negative \
                     means you are carrying fatigue.",
                )
                .build(),
        );

        let hero_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        let tsb_label = gtk::Label::builder()
            .label("—")
            .css_classes(["display", "numeric"])
            .halign(gtk::Align::Start)
            .build();
        hero_row.append(&tsb_label);

        let hero_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        let form_phrase = gtk::Label::builder()
            .label("Complete a workout to start tracking form")
            .css_classes(["title-3"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        let ctl_atl_pair = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .build();
        hero_text.append(&form_phrase);
        hero_text.append(&ctl_atl_pair);
        hero_row.append(&hero_text);
        root.append(&hero_row);

        let icu_indicator = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "dim-label"])
            .halign(gtk::Align::Start)
            .visible(false)
            .build();

        let pmc_data: Rc<RefCell<Vec<PmcPoint>>> = Rc::new(RefCell::new(Vec::new()));
        let pmc_chart = Self::build_chart(&pmc_data);

        // Legend mirrors the drawing: CTL is genuinely accent-coloured, the
        // rest are neutral line styles.
        let pmc_legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(18)
            .build();
        for (label, css) in [
            ("━ Fitness (CTL)", "accent"),
            ("╌ Fatigue (ATL)", "dim-label"),
            ("▒ Form (TSB)", "dim-label"),
        ] {
            pmc_legend.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption", css])
                    .build(),
            );
        }

        let pmc_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        pmc_section.append(&pmc_chart);
        pmc_section.append(&pmc_legend);
        pmc_section.append(&icu_indicator);
        root.append(&pmc_section);

        Self {
            root,
            tsb_label,
            form_phrase,
            ctl_atl_pair,
            icu_indicator,
            pmc_section,
            pmc_chart,
            pmc_data,
        }
    }

    /// The performance-management chart: CTL and ATL as lines, TSB as a fill.
    ///
    /// The `accent` style class is what makes `widget.color()` resolve to the
    /// GNOME accent colour (libadwaita 1.5 has no accent API); the neutral
    /// foreground for supporting strokes comes from the parent widget instead.
    fn build_chart(data: &Rc<RefCell<Vec<PmcPoint>>>) -> gtk::DrawingArea {
        let chart = gtk::DrawingArea::builder()
            .content_height(170)
            .hexpand(true)
            .css_classes(["accent"])
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        chart.update_property(&[gtk::accessible::Property::Label(
            "Performance Management Chart: 90-day history of fitness (CTL), fatigue (ATL), and form (TSB)",
        )]);

        let data = Rc::clone(data);
        chart.set_draw_func(move |widget, cr, width, height| {
            let points = data.borrow();
            if points.len() < 2 {
                return;
            }

            let accent = widget.color();
            let (ar, ag, ab) = (
                accent.red() as f64,
                accent.green() as f64,
                accent.blue() as f64,
            );
            let fg = widget.parent().map(|p| p.color()).unwrap_or(accent);
            let (fr, fgr, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let w = width as f64;
            let h = height as f64;
            let n = points.len();

            let (y_min, y_max) = plot_range(&points);
            let y_span = (y_max - y_min).max(1.0);
            let usable = h - AXIS_PAD_TOP - AXIS_PAD_BOTTOM;

            let x_at = |i: usize| i as f64 / (n - 1).max(1) as f64 * w;
            let y_at = |v: f64| AXIS_PAD_TOP + (1.0 - (v - y_min) / y_span) * usable;

            // Zero line (thin, dimmed)
            let zero_y = y_at(0.0);
            cr.set_source_rgba(fr, fgr, fb, 0.25);
            cr.set_line_width(1.0);
            cr.move_to(0.0, zero_y);
            cr.line_to(w, zero_y);
            cr.stroke().ok();

            // TSB as a soft fill against the zero line — no third line needed
            cr.new_path();
            cr.move_to(x_at(0), zero_y);
            for (i, point) in points.iter().enumerate() {
                cr.line_to(x_at(i), y_at(point.tsb));
            }
            cr.line_to(x_at(n - 1), zero_y);
            cr.close_path();
            cr.set_source_rgba(fr, fgr, fb, 0.08);
            cr.fill().ok();

            let draw_series = |value: fn(&PmcPoint) -> f64| {
                cr.move_to(x_at(0), y_at(value(&points[0])));
                for (i, point) in points.iter().enumerate().skip(1) {
                    cr.line_to(x_at(i), y_at(value(point)));
                }
                cr.stroke().ok();
            };

            // ATL: thin dashed — draw first so CTL renders on top
            cr.set_source_rgba(fr, fgr, fb, 0.45);
            cr.set_line_width(1.5);
            cr.set_dash(&[4.0, 3.0], 0.0);
            draw_series(|p| p.atl);
            cr.set_dash(&[], 0.0);

            // CTL: the bold headline series, in the accent colour
            cr.set_source_rgba(ar, ag, ab, 0.90);
            cr.set_line_width(2.5);
            draw_series(|p| p.ctl);

            // Mark today on the CTL line
            if let Some(last) = points.last() {
                cr.arc(x_at(n - 1), y_at(last.ctl), 3.5, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }

            // X-axis: a tick and short month label at the 1st of each month
            cr.set_source_rgba(fr, fgr, fb, 0.55);
            cr.set_font_size(10.0);
            let axis_y = h - AXIS_PAD_BOTTOM + 4.0;
            let label_y = h - 4.0;
            for (i, point) in points.iter().enumerate() {
                if point.date.day() == 1 {
                    let x = x_at(i);
                    cr.set_line_width(1.0);
                    cr.move_to(x, axis_y - 4.0);
                    cr.line_to(x, axis_y);
                    cr.stroke().ok();
                    cr.move_to(x + 2.0, label_y);
                    cr.show_text(&point.date.format("%b").to_string()).ok();
                }
            }
        });

        chart
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Update the headline numbers. Shows the placeholder state when the rider
    /// has no meaningful load yet.
    pub fn set_form(&self, ctl: f64, atl: f64, tsb: f64) {
        let has_load = ctl > 0.5 || atl > 0.5;
        if has_load {
            self.tsb_label.set_label(&format!("{:+.0}", tsb));
            self.form_phrase.set_label(tsb_status_text(tsb));
            self.ctl_atl_pair
                .set_label(&format!("Fitness {:.0} · Fatigue {:.0}", ctl, atl));
        } else {
            self.tsb_label.set_label("—");
            self.form_phrase
                .set_label("Complete a workout to start tracking form");
            self.ctl_atl_pair.set_label("");
        }

        // TSB value colour — genuinely semantic: fresh is good, deep fatigue
        // warrants attention.
        self.tsb_label.remove_css_class("success");
        self.tsb_label.remove_css_class("warning");
        let band = TsbBand::of(tsb);
        if has_load && band.is_fresh() {
            self.tsb_label.add_css_class("success");
        } else if has_load && band.is_fatigued() {
            self.tsb_label.add_css_class("warning");
        }
    }

    /// Replace the PMC series. Fewer than two days cannot make a chart, so the
    /// whole section hides rather than showing an empty plot.
    pub fn set_pmc_series(&self, points: Vec<PmcPoint>) {
        let has_chart = points.len() >= 2;
        self.pmc_section.set_visible(has_chart);
        if has_chart {
            *self.pmc_data.borrow_mut() = points;
            self.pmc_chart.queue_draw();
        }
    }

    /// Note how many of the plotted activities came from Intervals.icu, so the
    /// numbers are not mistaken for in-app rides alone.
    pub fn set_synced_count(&self, count: usize) {
        if count > 0 {
            self.icu_indicator.set_label(&format!(
                "Includes {count} activities synced from Intervals.icu"
            ));
            self.icu_indicator.set_visible(true);
        } else {
            self.icu_indicator.set_visible(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn point(ctl: f64, atl: f64) -> PmcPoint {
        PmcPoint {
            date: NaiveDate::from_ymd_opt(2026, 1, 1).expect("hardcoded valid date"),
            ctl,
            atl,
            tsb: ctl - atl,
        }
    }

    #[test]
    fn should_always_include_zero_so_the_form_fill_has_a_baseline() {
        let (min, max) = plot_range(&[point(40.0, 30.0), point(50.0, 35.0)]);
        assert!(min <= 0.0);
        assert!(max >= 10.0);
    }

    #[test]
    fn should_keep_a_minimum_height_for_a_barely_trained_athlete() {
        // Without the floor, a 2-unit CTL would fill the whole chart height.
        let (min, max) = plot_range(&[point(1.0, 0.5), point(2.0, 1.0)]);
        assert_eq!((min, max), (0.0, 10.0));
    }

    #[test]
    fn should_stretch_to_fit_the_highest_fitness() {
        let (_, max) = plot_range(&[point(40.0, 30.0), point(95.0, 60.0)]);
        assert_eq!(max, 95.0);
    }

    #[test]
    fn should_stretch_below_zero_to_fit_deep_fatigue() {
        // A rider deep in the hole: TSB well negative must stay on the chart.
        let (min, _) = plot_range(&[point(50.0, 90.0)]);
        assert_eq!(min, -40.0);
    }

    #[test]
    fn should_have_a_usable_range_for_an_empty_series() {
        let (min, max) = plot_range(&[]);
        assert!(max > min, "an empty chart must not divide by zero");
    }
}
