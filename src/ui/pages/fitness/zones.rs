//! "Where the Time Goes" — how training time splits across power and HR zones.

use adw::prelude::*;

use crate::ui::widgets::zone_bar::ZoneBar;

/// Heart-rate zone names. Power zones are numbered only, since Z1–Z7 is the
/// universal shorthand, but HR zones benefit from the reminder.
const HR_ZONE_LABELS: [&str; 5] = [
    "Z1 Easy",
    "Z2 Aerobic",
    "Z3 Tempo",
    "Z4 Threshold",
    "Z5 Max",
];

const POWER_ZONE_LABELS: [&str; 7] = ["Z1", "Z2", "Z3", "Z4", "Z5", "Z6", "Z7"];

/// One labelled zone bar with its own heading and legend.
struct LabelledZoneBar {
    root: gtk::Box,
    bar: ZoneBar,
}

impl LabelledZoneBar {
    fn new(title: &str, tooltip: &str, accessible_label: &str, labels: &[&str]) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        root.append(
            &gtk::Label::builder()
                .label(title)
                .halign(gtk::Align::Start)
                .css_classes(["caption-heading", "dim-label"])
                .tooltip_text(tooltip)
                .build(),
        );

        let bar = ZoneBar::new(accessible_label);
        root.append(bar.widget());

        let legend = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .homogeneous(true)
            .build();
        for label in labels {
            legend.append(
                &gtk::Label::builder()
                    .label(*label)
                    .css_classes(["caption", "dim-label"])
                    .halign(gtk::Align::Center)
                    .build(),
            );
        }
        root.append(&legend);

        Self { root, bar }
    }

    /// Feed the bar. Returns whether there was anything to show, and hides
    /// itself when there was not.
    fn set_seconds(&self, seconds: &[u32]) -> bool {
        let has_data = seconds.iter().any(|&s| s > 0);
        self.root.set_visible(has_data);
        if has_data {
            self.bar.set_seconds(seconds);
        }
        has_data
    }
}

/// The zone-distribution section: power on top, heart rate below.
pub struct ZonesSection {
    root: gtk::Box,
    power: LabelledZoneBar,
    heart_rate: LabelledZoneBar,
}

impl ZonesSection {
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .visible(false)
            .build();
        root.append(
            &gtk::Label::builder()
                .label("Where the Time Goes")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build(),
        );

        let power = LabelledZoneBar::new(
            "Power",
            "Time spent in each power zone, all recorded sessions. Endurance \
             athletes typically aim for 70–80% in Z1–Z2 (polarised) or Z2–Z3 \
             (pyramidal)",
            "Power zone distribution bar: proportional time in zones Z1 through Z7",
            &POWER_ZONE_LABELS,
        );
        root.append(&power.root);

        let heart_rate = LabelledZoneBar::new(
            "Heart rate",
            "Time in each HR zone based on your recorded max HR — in-app sessions only",
            "Heart rate zone distribution bar: proportional time in HR zones Z1 through Z5",
            &HR_ZONE_LABELS,
        );
        root.append(&heart_rate.root);

        Self {
            root,
            power,
            heart_rate,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Update both bars. The section hides when neither has anything to show.
    pub fn set_zones(&self, power_seconds: &[u32; 7], hr_seconds: &[u32; 5]) {
        let has_power = self.power.set_seconds(power_seconds);
        let has_hr = self.heart_rate.set_seconds(hr_seconds);
        self.root.set_visible(has_power || has_hr);
    }
}
