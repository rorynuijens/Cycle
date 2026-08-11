//! Load history: how hard the last six weeks were, and how much work they took.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::training::analytics::VolumeTotals;

/// How many weeks the training-stress bar chart shows.
pub const TSS_WEEKS: i64 = 6;

/// Horizontal space between bars, in pixels.
const BAR_GAP: f64 = 6.0;

/// Shortest visible bar, so a very easy week still reads as "something".
const MIN_BAR_HEIGHT: f64 = 2.0;

/// Width of one bar so that `count` bars plus their gaps fill `width`.
fn bar_width(count: usize, width: f64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    let n = count as f64;
    ((width - BAR_GAP * (n - 1.0)) / n).max(1.0)
}

/// Height of a bar scaled against the tallest week, or `None` for a week with
/// no training at all — an empty week draws its track and nothing else.
fn bar_height(tss: f32, max_tss: f32, height: f64) -> Option<f64> {
    if max_tss <= 0.0 || tss <= 0.0 {
        return None;
    }
    Some(((tss as f64 / max_tss as f64) * height).max(MIN_BAR_HEIGHT))
}

/// The Load History section: weekly TSS bars over a volume summary strip.
pub struct LoadHistory {
    root: gtk::Box,
    chart: gtk::DrawingArea,
    weekly_tss: Rc<RefCell<Vec<f32>>>,
    week_labels: Vec<gtk::Label>,
    tss_labels: Vec<gtk::Label>,
    volume_section: gtk::Box,
    week_kj: gtk::Label,
    week_hours: gtk::Label,
    month_kj: gtk::Label,
    total_sessions: gtk::Label,
}

impl LoadHistory {
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        root.append(
            &gtk::Label::builder()
                .label("Load History")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .tooltip_text(
                    "Training Stress Score per week for the past 6 weeks — higher bars \
                     mean harder weeks. A sustainable build is roughly 5–10% per week.",
                )
                .build(),
        );

        let weekly_tss: Rc<RefCell<Vec<f32>>> =
            Rc::new(RefCell::new(vec![0.0; TSS_WEEKS as usize]));
        let chart = Self::build_chart(&weekly_tss);
        root.append(&chart);

        // Two rows under the bars: which week, and what it scored.
        let week_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .homogeneous(true)
            .build();
        let tss_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .homogeneous(true)
            .build();
        let mut week_labels = Vec::with_capacity(TSS_WEEKS as usize);
        let mut tss_labels = Vec::with_capacity(TSS_WEEKS as usize);
        for _ in 0..TSS_WEEKS {
            let week = gtk::Label::builder()
                .label("")
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Center)
                .build();
            week_row.append(&week);
            week_labels.push(week);

            let tss = gtk::Label::builder()
                .label("")
                .css_classes(["caption", "numeric"])
                .halign(gtk::Align::Center)
                .build();
            tss_row.append(&tss);
            tss_labels.push(tss);
        }
        root.append(&week_row);
        root.append(&tss_row);

        let volume_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(24)
            .margin_top(6)
            .visible(false)
            .tooltip_text(
                "Kilojoules measure total mechanical work done — a more accurate proxy \
                 for training load than time alone",
            )
            .build();
        let (week_kj_col, week_kj) = Self::make_stat("This week");
        let (week_hrs_col, week_hours) = Self::make_stat("Time this week");
        let (month_kj_col, month_kj) = Self::make_stat("This month");
        let (sessions_col, total_sessions) = Self::make_stat("Sessions");
        volume_section.append(&week_kj_col);
        volume_section.append(&week_hrs_col);
        volume_section.append(&month_kj_col);
        volume_section.append(&sessions_col);
        root.append(&volume_section);

        Self {
            root,
            chart,
            weekly_tss,
            week_labels,
            tss_labels,
            volume_section,
            week_kj,
            week_hours,
            month_kj,
            total_sessions,
        }
    }

    /// The weekly TSS bars: a dim track per week, filled to that week's load.
    ///
    /// The `accent` style class is what makes `widget.color()` resolve to the
    /// GNOME accent colour — libadwaita 1.5 exposes no accent API.
    fn build_chart(data: &Rc<RefCell<Vec<f32>>>) -> gtk::DrawingArea {
        let chart = gtk::DrawingArea::builder()
            .content_height(120)
            .hexpand(true)
            .css_classes(["accent"])
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        chart.update_property(&[gtk::accessible::Property::Label(
            "Weekly TSS bar chart: training stress score for the past 6 weeks",
        )]);

        let data = Rc::clone(data);
        chart.set_draw_func(move |widget, cr, width, height| {
            let weeks = data.borrow();
            let max_tss = weeks.iter().copied().fold(0.0f32, f32::max);
            let fg = widget.color();
            let (r, g, b) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
            let h = height as f64;
            let bar_w = bar_width(weeks.len(), width as f64);

            for (i, &tss) in weeks.iter().enumerate() {
                let x = i as f64 * (bar_w + BAR_GAP);
                cr.set_source_rgba(r, g, b, 0.10);
                cr.rectangle(x, 0.0, bar_w, h);
                cr.fill().ok();

                if let Some(bar_h) = bar_height(tss, max_tss, h) {
                    cr.set_source_rgba(r, g, b, 0.65);
                    cr.rectangle(x, h - bar_h, bar_w, bar_h);
                    cr.fill().ok();
                }
            }
        });

        chart
    }

    /// One labelled figure in the volume strip.
    fn make_stat(caption: &str) -> (gtk::Box, gtk::Label) {
        let col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        col.append(
            &gtk::Label::builder()
                .label(caption)
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let value = gtk::Label::builder()
            .label("—")
            .halign(gtk::Align::Start)
            .css_classes(["title-4", "numeric"])
            .build();
        col.append(&value);
        (col, value)
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Replace the bars and their labels. `weeks` runs oldest to newest.
    pub fn set_weekly_tss(&self, weeks: &[(String, f32)]) {
        for ((week_lbl, tss_lbl), (name, tss)) in self
            .week_labels
            .iter()
            .zip(self.tss_labels.iter())
            .zip(weeks.iter())
        {
            week_lbl.set_label(name);
            if *tss > 0.0 {
                tss_lbl.set_label(&format!("{tss:.0}"));
            } else {
                tss_lbl.set_label("—");
            }
        }
        *self.weekly_tss.borrow_mut() = weeks.iter().map(|(_, tss)| *tss).collect();
        self.chart.queue_draw();
    }

    /// Update the work/time strip. Hides it entirely until there is a first ride.
    pub fn set_volume(&self, volume: &VolumeTotals) {
        self.volume_section.set_visible(volume.activity_count > 0);
        self.week_kj.set_label(&format!("{:.0} kJ", volume.week_kj));
        self.week_hours
            .set_label(&format!("{:.1} h", volume.week_secs as f32 / 3600.0));
        self.month_kj
            .set_label(&format!("{:.0} kJ", volume.month_kj));
        self.total_sessions
            .set_label(&volume.activity_count.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_fill_the_width_with_bars_and_gaps() {
        let width = 366.0;
        let bar_w = bar_width(6, width);
        let drawn = bar_w * 6.0 + BAR_GAP * 5.0;
        assert!((drawn - width).abs() < 1e-9, "bars must tile the chart");
    }

    #[test]
    fn should_keep_bars_visible_in_a_narrow_window() {
        // Gaps alone would exceed the width; the bar must not go negative.
        assert!(bar_width(6, 10.0) >= 1.0);
    }

    #[test]
    fn should_have_no_bars_without_weeks() {
        assert_eq!(bar_width(0, 300.0), 0.0);
    }

    #[test]
    fn should_draw_the_hardest_week_at_full_height() {
        assert_eq!(bar_height(400.0, 400.0, 120.0), Some(120.0));
    }

    #[test]
    fn should_scale_an_easier_week_proportionally() {
        assert_eq!(bar_height(200.0, 400.0, 120.0), Some(60.0));
    }

    #[test]
    fn should_draw_no_bar_for_a_week_off() {
        assert_eq!(bar_height(0.0, 400.0, 120.0), None);
    }

    #[test]
    fn should_draw_no_bars_when_no_week_had_any_load() {
        assert_eq!(bar_height(0.0, 0.0, 120.0), None);
    }

    #[test]
    fn should_keep_a_very_easy_week_visible() {
        // 1 TSS against a 400 TSS week would round to a sub-pixel sliver.
        let h = bar_height(1.0, 400.0, 120.0).expect("a ridden week draws a bar");
        assert!(h >= MIN_BAR_HEIGHT, "got {h}");
    }
}
