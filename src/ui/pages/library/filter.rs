//! What the Library's chips and search leave on screen.
//!
//! One rule for workouts and routes alike, so a search for a name finds it
//! whichever kind it is, and a chip means the same thing next to the others.

use std::collections::HashSet;

use crate::data::workout::{Workout, WorkoutCategory};

/// The chips and search text in force, as one decision.
pub struct Filter<'a> {
    categories: &'a HashSet<WorkoutCategory>,
    routes: bool,
    search: String,
}

impl<'a> Filter<'a> {
    /// `routes` is the Routes chip; `search` is the text as typed.
    pub fn new(categories: &'a HashSet<WorkoutCategory>, routes: bool, search: &str) -> Self {
        Self {
            categories,
            routes,
            search: fold(search.trim()),
        }
    }

    /// No chip selected means no chip filter, not "show nothing".
    fn any_chip(&self) -> bool {
        self.routes || !self.categories.is_empty()
    }

    fn name_matches(&self, name: &str) -> bool {
        self.search.is_empty() || fold(name).contains(&self.search)
    }

    pub fn shows_workout(&self, workout: &Workout) -> bool {
        (!self.any_chip() || self.categories.contains(&workout.category))
            && self.name_matches(&workout.name)
    }

    pub fn shows_route(&self, name: &str) -> bool {
        (!self.any_chip() || self.routes) && self.name_matches(name)
    }
}

/// Lower-case `text` for searching, treating the Turkish dotted and dotless
/// I as plain `i`.
///
/// Plain `to_lowercase` is not enough: `İ` lowers to `i` plus a combining dot,
/// so "İzmir" never contains "izmir", and `ı` stays `ı`, so "Kadıköy" never
/// contains "kadikoy" typed on a keyboard without it — while "KADIKÖY", which
/// is how the same name reads in capitals, lowers to "kadiköy" and misses too.
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|&c| c != '\u{307}')
        .map(|c| if c == 'ı' { 'i' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workout::Segment;

    fn workout(name: &str, category: WorkoutCategory) -> Workout {
        Workout {
            id: 1,
            name: name.into(),
            description: String::new(),
            duration_secs: 600,
            tss: 10.0,
            category,
            segments: vec![Segment::steady(600, 60.0, "Steady")],
        }
    }

    fn cats(list: &[WorkoutCategory]) -> HashSet<WorkoutCategory> {
        list.iter().copied().collect()
    }

    #[test]
    fn should_show_everything_with_no_chips_and_no_search() {
        let none = cats(&[]);
        let f = Filter::new(&none, false, "");
        assert!(f.shows_workout(&workout("Over-Unders", WorkoutCategory::Threshold)));
        assert!(f.shows_route("Alpe d'Huez"));
    }

    #[test]
    fn should_hide_routes_when_only_a_workout_chip_is_on() {
        let threshold = cats(&[WorkoutCategory::Threshold]);
        let f = Filter::new(&threshold, false, "");
        assert!(f.shows_workout(&workout("Over-Unders", WorkoutCategory::Threshold)));
        assert!(!f.shows_workout(&workout("Spin", WorkoutCategory::Recovery)));
        assert!(!f.shows_route("Alpe d'Huez"));
    }

    #[test]
    fn should_hide_every_workout_when_only_the_routes_chip_is_on() {
        let none = cats(&[]);
        let f = Filter::new(&none, true, "");
        assert!(f.shows_route("Alpe d'Huez"));
        assert!(!f.shows_workout(&workout("Over-Unders", WorkoutCategory::Threshold)));
    }

    #[test]
    fn should_show_both_kinds_when_both_chips_are_on() {
        let threshold = cats(&[WorkoutCategory::Threshold]);
        let f = Filter::new(&threshold, true, "");
        assert!(f.shows_route("Alpe d'Huez"));
        assert!(f.shows_workout(&workout("Over-Unders", WorkoutCategory::Threshold)));
        assert!(!f.shows_workout(&workout("Spin", WorkoutCategory::Recovery)));
    }

    #[test]
    fn should_search_route_names_regardless_of_case() {
        let none = cats(&[]);
        assert!(Filter::new(&none, false, "alpe").shows_route("Alpe d'Huez"));
        assert!(Filter::new(&none, false, "HUEZ").shows_route("Alpe d'Huez"));
        assert!(!Filter::new(&none, false, "ventoux").shows_route("Alpe d'Huez"));
    }

    #[test]
    fn should_search_within_the_chip_the_rider_picked() {
        // The search narrows the chips; it does not reach past them.
        let none = cats(&[]);
        let f = Filter::new(&none, true, "alpe");
        assert!(f.shows_route("Alpe d'Huez"));
        assert!(!f.shows_workout(&workout("Alpe Repeats", WorkoutCategory::Vo2Max)));
    }

    #[test]
    fn should_find_a_turkish_name_typed_without_its_dotted_capital() {
        let none = cats(&[]);
        for typed in ["izmir", "IZMIR", "İzmir", "İZMİR"] {
            assert!(
                Filter::new(&none, false, typed).shows_route("İzmir"),
                "{typed}"
            );
        }
    }

    #[test]
    fn should_find_a_dotless_i_typed_as_a_plain_one() {
        let none = cats(&[]);
        for typed in ["kadıköy", "KADIKÖY", "kadiköy"] {
            assert!(
                Filter::new(&none, false, typed).shows_route("Kadıköy Sahil"),
                "{typed}"
            );
        }
    }

    #[test]
    fn should_ignore_spaces_around_the_search() {
        // A trailing space from typing "alpe " must not empty the list.
        let none = cats(&[]);
        assert!(Filter::new(&none, false, "  alpe ").shows_route("Alpe d'Huez"));
        assert!(Filter::new(&none, false, "   ").shows_route("Alpe d'Huez"));
    }

    #[test]
    fn should_keep_the_space_inside_a_search() {
        let none = cats(&[]);
        assert!(Filter::new(&none, false, "alpe d").shows_route("Alpe d'Huez"));
        assert!(!Filter::new(&none, false, "alped").shows_route("Alpe d'Huez"));
    }
}
