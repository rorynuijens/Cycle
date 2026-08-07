//! The wellness grid: six sparkline cards fed by Intervals.icu sync.
//!
//! Every card works the same way — a latest reading, a trend against the
//! rider's own recent average, and a 14-day sparkline — so they are described
//! once in [`CARDS`] and built from that description.

use adw::prelude::*;
use chrono::NaiveDate;

use crate::data::db::WellnessEntry;
use crate::training::analytics::build_wellness_series;
use crate::ui::widgets::sparkline::Sparkline;

/// A reading must differ from the average by more than this to count as a
/// move rather than noise.
const NOISE_THRESHOLD: f32 = 0.03;

/// Readings needed before a trend means anything.
const MIN_READINGS_FOR_TREND: usize = 3;

/// One wellness metric: what it is called, and how to get it out of an entry.
struct CardSpec {
    title: &'static str,
    unit: &'static str,
    /// Decimal places for the headline value.
    decimals: usize,
    /// Whether a reading above average is good news (HRV) or bad (resting HR).
    higher_is_better: bool,
    extract: fn(&WellnessEntry) -> Option<f32>,
}

/// The six metrics shown, in display order.
const CARDS: [CardSpec; 6] = [
    CardSpec {
        title: "HRV",
        unit: "",
        decimals: 0,
        higher_is_better: true,
        extract: |e| e.hrv,
    },
    CardSpec {
        title: "Resting Heart Rate",
        unit: "bpm",
        decimals: 0,
        // A resting heart rate above your own average is a fatigue signal.
        higher_is_better: false,
        extract: |e| e.resting_hr.map(|v| v as f32),
    },
    CardSpec {
        title: "Sleep",
        unit: "hours",
        decimals: 1,
        higher_is_better: true,
        extract: |e| e.sleep_secs.map(|s| s as f32 / 3600.0),
    },
    CardSpec {
        title: "Sleep Score",
        unit: "/ 100",
        decimals: 0,
        higher_is_better: true,
        extract: |e| e.sleep_score.map(|v| v as f32),
    },
    CardSpec {
        title: "Steps",
        unit: "today",
        decimals: 0,
        higher_is_better: true,
        extract: |e| e.steps.map(|v| v as f32),
    },
    CardSpec {
        title: "Calories",
        unit: "kcal",
        decimals: 0,
        higher_is_better: true,
        extract: |e| e.calories.map(|v| v as f32),
    },
];

/// How the latest reading compares with the rider's own recent average.
#[derive(Debug, PartialEq)]
enum Trend {
    /// Too few readings to compare against.
    Unknown,
    /// Within [`NOISE_THRESHOLD`] of average.
    Average,
    /// Above average, by this percentage.
    Above(f32),
    /// Below average, by this percentage.
    Below(f32),
}

impl Trend {
    /// Compare the latest reading against the mean of every reading in the
    /// series. Days with no data (`0.0`) are excluded from the average.
    fn of(series: &[f32]) -> Self {
        let readings: Vec<f32> = series.iter().filter(|&&v| v > 0.0).copied().collect();
        let Some(&current) = readings.last() else {
            return Trend::Unknown;
        };
        if readings.len() < MIN_READINGS_FOR_TREND {
            return Trend::Unknown;
        }

        let average = readings.iter().sum::<f32>() / readings.len() as f32;
        let delta = current - average;
        if delta.abs() < average * NOISE_THRESHOLD {
            return Trend::Average;
        }

        let pct = (delta / average * 100.0).abs();
        if delta > 0.0 {
            Trend::Above(pct)
        } else {
            Trend::Below(pct)
        }
    }

    fn label(&self) -> String {
        match self {
            Trend::Unknown => String::new(),
            Trend::Average => "→ avg".to_string(),
            Trend::Above(pct) => format!("↑ {pct:.0}% above avg"),
            Trend::Below(pct) => format!("↓ {pct:.0}% below avg"),
        }
    }

    /// The Adwaita status class for this trend, given which direction is good.
    fn css_class(&self, higher_is_better: bool) -> Option<&'static str> {
        let good = |is_good: bool| Some(if is_good { "success" } else { "warning" });
        match self {
            Trend::Unknown | Trend::Average => None,
            Trend::Above(_) => good(higher_is_better),
            Trend::Below(_) => good(!higher_is_better),
        }
    }
}

/// The most recent day with an actual reading.
fn latest_reading(series: &[f32]) -> Option<f32> {
    series.iter().rev().find(|&&v| v > 0.0).copied()
}

/// One metric's card: headline value, trend, and sparkline.
struct WellnessCard {
    root: gtk::Box,
    value: gtk::Label,
    trend: gtk::Label,
    chart: Sparkline,
}

impl WellnessCard {
    fn new(spec: &CardSpec) -> Self {
        let root = gtk::Box::builder()
            .css_classes(["card"])
            .hexpand(true)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        vbox.append(
            &gtk::Label::builder()
                .label(spec.title)
                .halign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build(),
        );

        let value = gtk::Label::builder()
            .label("—")
            .halign(gtk::Align::Start)
            .css_classes(["title-2", "numeric"])
            .build();
        vbox.append(&value);

        if !spec.unit.is_empty() {
            vbox.append(
                &gtk::Label::builder()
                    .label(spec.unit)
                    .halign(gtk::Align::Start)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }

        let chart = Sparkline::new();
        vbox.append(chart.widget());

        let trend = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .css_classes(["caption"])
            .build();
        vbox.append(&trend);

        root.append(&vbox);
        Self {
            root,
            value,
            trend,
            chart,
        }
    }

    fn set_series(&self, series: &[f32], spec: &CardSpec) {
        match latest_reading(series) {
            Some(v) => self.value.set_label(&format!("{:.*}", spec.decimals, v)),
            None => self.value.set_label("—"),
        }

        let trend = Trend::of(series);
        self.trend.remove_css_class("success");
        self.trend.remove_css_class("warning");
        self.trend.set_label(&trend.label());
        if let Some(class) = trend.css_class(spec.higher_is_better) {
            self.trend.add_css_class(class);
        }

        self.chart.set_values(series);
    }
}

/// The wellness section: a responsive grid of cards, or an explanation of why
/// there is nothing to show.
pub struct WellnessGrid {
    root: gtk::Box,
    flow: gtk::FlowBox,
    no_data: gtk::Label,
    cards: Vec<WellnessCard>,
}

impl WellnessGrid {
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        root.append(
            &gtk::Label::builder()
                .label("Wellness")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .tooltip_text(
                    "HRV, resting heart rate, sleep, and activity data synced from \
                     Intervals.icu (Preferences → Intervals.icu)",
                )
                .build(),
        );

        let no_data = gtk::Label::builder()
            .label(
                "No wellness data yet — sync Intervals.icu in Preferences to see \
                 HRV, sleep, and step data from your connected devices.",
            )
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .visible(false)
            .build();
        root.append(&no_data);

        let flow = gtk::FlowBox::builder()
            .column_spacing(12)
            .row_spacing(12)
            .max_children_per_line(2)
            .min_children_per_line(1)
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .build();

        let cards: Vec<WellnessCard> = CARDS.iter().map(WellnessCard::new).collect();
        for card in &cards {
            flow.append(&card.root);
        }
        for i in 0..cards.len() as i32 {
            if let Some(child) = flow.child_at_index(i) {
                child.set_hexpand(true);
            }
        }
        root.append(&flow);

        Self {
            root,
            flow,
            no_data,
            cards,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Rebuild every card from the synced wellness history.
    ///
    /// With nothing synced the grid hides entirely — six empty cards would
    /// read as "all your readings are zero" rather than "nothing to show".
    pub fn set_entries(&self, entries: &[WellnessEntry], today: NaiveDate) {
        let series: Vec<Vec<f32>> = CARDS
            .iter()
            .map(|spec| build_wellness_series(entries, today, spec.extract))
            .collect();

        let has_data = series.iter().any(|s| s.iter().any(|&v| v > 0.0));
        self.no_data.set_visible(!has_data);
        self.flow.set_visible(has_data);
        if !has_data {
            return;
        }

        for ((card, spec), series) in self.cards.iter().zip(CARDS.iter()).zip(series.iter()) {
            card.set_series(series, spec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_no_trend_without_enough_readings() {
        assert_eq!(Trend::of(&[50.0, 52.0]), Trend::Unknown);
    }

    #[test]
    fn should_report_no_trend_for_an_empty_series() {
        assert_eq!(Trend::of(&[]), Trend::Unknown);
        assert_eq!(Trend::of(&[0.0, 0.0, 0.0, 0.0]), Trend::Unknown);
    }

    #[test]
    fn should_ignore_days_with_no_reading_when_averaging() {
        // Gaps must not drag the average towards zero.
        assert_eq!(Trend::of(&[50.0, 0.0, 50.0, 0.0, 50.0]), Trend::Average);
    }

    #[test]
    fn should_call_a_small_move_average() {
        // 51 against an average of ~50.3 is inside the 3% noise band.
        assert_eq!(Trend::of(&[50.0, 50.0, 51.0]), Trend::Average);
    }

    #[test]
    fn should_report_a_reading_above_average() {
        // Readings 40, 40, 60 average 46.67; 60 is ~28.6% above.
        match Trend::of(&[40.0, 40.0, 60.0]) {
            Trend::Above(pct) => assert!((pct - 28.57).abs() < 0.1, "got {pct}"),
            other => panic!("expected Above, got {other:?}"),
        }
    }

    #[test]
    fn should_report_a_reading_below_average() {
        match Trend::of(&[60.0, 60.0, 40.0]) {
            Trend::Below(pct) => assert!((pct - 25.0).abs() < 0.1, "got {pct}"),
            other => panic!("expected Below, got {other:?}"),
        }
    }

    #[test]
    fn should_use_the_latest_reading_not_the_largest() {
        // A peak earlier in the window must not be mistaken for today.
        assert_eq!(latest_reading(&[10.0, 99.0, 20.0]), Some(20.0));
    }

    #[test]
    fn should_skip_trailing_gaps_when_finding_the_latest_reading() {
        assert_eq!(latest_reading(&[10.0, 20.0, 0.0, 0.0]), Some(20.0));
    }

    #[test]
    fn should_find_no_reading_in_an_empty_window() {
        assert_eq!(latest_reading(&[0.0, 0.0]), None);
    }

    #[test]
    fn should_praise_a_high_reading_only_when_high_is_good() {
        let high = Trend::Above(20.0);
        // High HRV is good news; a high resting heart rate is not.
        assert_eq!(high.css_class(true), Some("success"));
        assert_eq!(high.css_class(false), Some("warning"));
    }

    #[test]
    fn should_flag_a_low_reading_only_when_high_is_good() {
        let low = Trend::Below(20.0);
        assert_eq!(low.css_class(true), Some("warning"));
        assert_eq!(low.css_class(false), Some("success"));
    }

    #[test]
    fn should_leave_an_average_reading_uncoloured() {
        assert_eq!(Trend::Average.css_class(true), None);
        assert_eq!(Trend::Unknown.css_class(false), None);
    }

    #[test]
    fn should_label_each_trend_for_the_card() {
        assert_eq!(Trend::Unknown.label(), "");
        assert_eq!(Trend::Average.label(), "→ avg");
        assert_eq!(Trend::Above(12.4).label(), "↑ 12% above avg");
        assert_eq!(Trend::Below(7.6).label(), "↓ 8% below avg");
    }

    #[test]
    fn should_show_sleep_to_one_decimal_and_the_rest_whole() {
        let sleep = CARDS
            .iter()
            .find(|c| c.title == "Sleep")
            .expect("sleep card exists");
        assert_eq!(sleep.decimals, 1);
        let hrv = CARDS
            .iter()
            .find(|c| c.title == "HRV")
            .expect("HRV card exists");
        assert_eq!(hrv.decimals, 0);
    }

    #[test]
    fn should_treat_only_resting_heart_rate_as_lower_is_better() {
        let lower_better: Vec<&str> = CARDS
            .iter()
            .filter(|c| !c.higher_is_better)
            .map(|c| c.title)
            .collect();
        assert_eq!(lower_better, vec!["Resting Heart Rate"]);
    }

    #[test]
    fn should_convert_sleep_seconds_to_hours() {
        let sleep = CARDS
            .iter()
            .find(|c| c.title == "Sleep")
            .expect("sleep card exists");
        let entry = WellnessEntry {
            date: NaiveDate::from_ymd_opt(2026, 1, 1).expect("hardcoded valid date"),
            hrv: None,
            resting_hr: None,
            sleep_secs: Some(27_000), // 7.5 h
            sleep_score: None,
            steps: None,
            calories: None,
        };
        assert_eq!((sleep.extract)(&entry), Some(7.5));
    }
}
