//! The linked Mon–Sun strip that asks which days the rider trains.
//!
//! Two places ask that question — the AI program builder and the local
//! roll-over — and before this they would have asked it with two copies of the
//! seven buttons and two copies of the weekday-name mapping. The mapping is
//! unforgiving of a typo: [`crate::ai::context::day_name_to_offset`] turns any
//! name it does not recognise into Monday rather than dropping the session, so
//! a misspelt "wedensday" would silently pile a week's training onto one day.
//! One spelling of it, here.

use adw::prelude::*;
use chrono::Weekday;
use std::rc::Rc;

/// Fill a ticked day with the accent colour.
///
/// A `GtkToggleButton`'s native pressed state is a slight change of shade,
/// which is legible enough on a row of two but not on a strip of seven inside a
/// dialog — the rider could not tell at a glance which days they had picked.
/// The accent fill is what every other "this is chosen" surface in the app uses
/// (CLAUDE.md §1.6), and it follows the desktop's own accent colour in both
/// themes rather than naming one.
///
/// Added and removed rather than set through the builder: a builder's
/// `css_classes` **replaces** the default list, and dropping a
/// `GtkToggleButton`'s own classes takes its shape and its pressed state with
/// them.
fn set_accent(toggle: &gtk::ToggleButton) {
    if toggle.is_active() {
        toggle.add_css_class("suggested-action");
    } else {
        toggle.remove_css_class("suggested-action");
    }
}

/// The days of the week, as shown and as the database and prompts name them.
const DAYS: [(&str, Weekday); 7] = [
    ("Mon", Weekday::Mon),
    ("Tue", Weekday::Tue),
    ("Wed", Weekday::Wed),
    ("Thu", Weekday::Thu),
    ("Fri", Weekday::Fri),
    ("Sat", Weekday::Sat),
    ("Sun", Weekday::Sun),
];

/// Ticked when nothing better is known — the classic three-day week.
pub const DEFAULT_DAYS: [Weekday; 3] = [Weekday::Mon, Weekday::Wed, Weekday::Fri];

/// A linked group of weekday toggles.
///
/// Cloning gives a second handle on the same seven buttons, not a copy of the
/// selection — GTK objects are reference-counted, and callers rely on that to
/// read the toggles from inside a callback built before the strip is packed.
#[derive(Clone)]
pub struct DayToggles {
    root: gtk::Box,
    toggles: Rc<Vec<gtk::ToggleButton>>,
}

impl DayToggles {
    /// Build the strip with `preselected` already ticked.
    pub fn new(preselected: &[Weekday]) -> Self {
        // A linked toggle group — the calendar's Week|Month pattern, multi-select.
        // Native pressed state, no CSS hacks.
        let root = gtk::Box::builder().css_classes(["linked"]).build();
        let toggles: Vec<gtk::ToggleButton> = DAYS
            .iter()
            .map(|(label, day)| {
                let toggle = gtk::ToggleButton::builder()
                    .label(*label)
                    .tooltip_text(format!("Train on {label}"))
                    .active(preselected.contains(day))
                    .build();
                set_accent(&toggle);
                toggle.connect_toggled(set_accent);
                root.append(&toggle);
                toggle
            })
            .collect();

        Self {
            root,
            toggles: Rc::new(toggles),
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The ticked days, Monday first.
    pub fn selected(&self) -> Vec<Weekday> {
        self.toggles
            .iter()
            .zip(DAYS.iter())
            .filter(|(toggle, _)| toggle.is_active())
            .map(|(_, (_, day))| *day)
            .collect()
    }

    /// The ticked days as `programs.training_days` stores them:
    /// `"monday,wednesday,friday"`.
    ///
    /// Empty when nothing is ticked, which callers must refuse rather than
    /// store — a blank column means "unknown", and writing one for a rider who
    /// simply ticked nothing would record a guess as a fact.
    pub fn selected_csv(&self) -> String {
        self.selected()
            .iter()
            .map(|d| crate::ai::context::weekday_name(*d))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Run `f` with the number of ticked days whenever the selection changes,
    /// so a dialog can grey its confirm button while there are none.
    pub fn connect_changed(&self, f: impl Fn(usize) + 'static) {
        let f = Rc::new(f);
        for toggle in self.toggles.iter() {
            let toggles = Rc::clone(&self.toggles);
            let f = Rc::clone(&f);
            toggle.connect_toggled(move |_| {
                f(toggles.iter().filter(|t| t.is_active()).count());
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_spell_every_day_as_the_database_stores_them() {
        // The names must round-trip through the offset table the scheduler
        // uses, or a session quietly lands on Monday.
        for (_, day) in DAYS {
            let name = crate::ai::context::weekday_name(day);
            assert_eq!(
                crate::ai::context::day_name_to_offset(name),
                day.num_days_from_monday(),
                "{name} does not map back to itself"
            );
        }
    }

    #[test]
    fn should_default_to_a_three_day_week() {
        assert_eq!(
            DEFAULT_DAYS.to_vec(),
            vec![Weekday::Mon, Weekday::Wed, Weekday::Fri]
        );
    }
}
