//! The card that offers the FTP a ramp test came out at.
//!
//! Accept-only, by design. FTP is the number every target, zone and training-load
//! figure in the app is scaled to, so it changes when the rider says so and not
//! because a ride finished. Dismissing is simply not pressing the button: the
//! test is recorded either way, and the card is gone by the next ride.
//!
//! The arithmetic is in [`crate::training::ftp_test`]; this only says what it
//! found and asks.

use adw::prelude::*;
use std::rc::Rc;

use crate::training::engine::WorkoutEngine;
use crate::training::ftp_test::RampResult;

/// Said before the rider accepts: nothing has changed yet.
const OFFERED: &str = "Nothing has changed yet. Accepting this sets your FTP and records \
                       it in your history — every target and zone follows it.";
/// Said once they have.
const ACCEPTED: &str = "Your FTP is updated, and this test is in your FTP history.";

/// Fill `holder` with the result of a ramp test, or leave it empty and hidden.
///
/// `on_accept` is handed the new FTP and is what actually changes anything; the
/// card updates itself either way, so a caller with nowhere to reload from still
/// shows the right thing.
pub fn attach(holder: &gtk::Box, result: Option<RampResult>, on_accept: Rc<dyn Fn(u32)>) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    let Some(result) = result else {
        holder.set_visible(false);
        return;
    };
    holder.set_visible(true);
    holder.append(&build(result, on_accept));
}

fn build(result: RampResult, on_accept: Rc<dyn Fn(u32)>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Your ramp test")
        .description(OFFERED)
        .build();

    // No prefix icon: the rows say everything in words, and the one tried here
    // rendered as a missing-image placeholder in the offscreen shots. An icon
    // earns its place where it carries meaning the text does not — the way the
    // warning does in `integrity_notice` — and there is none to carry here.
    group.add(
        &adw::ActionRow::builder()
            .title(format!("FTP {} W", result.new_ftp))
            .subtitle(change_sentence(&result))
            .title_lines(0)
            .subtitle_lines(0)
            .use_markup(false)
            .build(),
    );

    group.add(
        &adw::ActionRow::builder()
            .title(format!("Best minute {} W", result.best_minute_watts))
            .subtitle(evidence_sentence(&result))
            .title_lines(0)
            .subtitle_lines(0)
            .use_markup(false)
            .build(),
    );

    let button = gtk::Button::builder()
        .label(format!("Set FTP to {} W", result.new_ftp))
        .css_classes(["suggested-action", "pill"])
        .valign(gtk::Align::Center)
        .tooltip_text("Use this as your FTP from now on")
        .build();

    // Updated in place rather than rebuilt: the handler runs on a widget this
    // group owns, and tearing that down underneath it is how re-entrancy bugs
    // start (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        // Weak, or it is a cycle: the group owns this button through its header
        // suffix, and a strong capture would own the group back.
        #[weak]
        group,
        move |btn| {
            btn.set_visible(false);
            group.set_description(Some(ACCEPTED));
            on_accept(result.new_ftp);
        }
    ));
    group.set_header_suffix(Some(&button));

    group
}

/// How the result compares with the FTP the test was ridden at.
fn change_sentence(result: &RampResult) -> String {
    if result.is_unchanged() {
        return format!(
            "The same as the {} W you are set to — the number you have is right.",
            result.previous_ftp
        );
    }
    let delta = result.delta_pct();
    let direction = if delta > 0.0 { "Up" } else { "Down" };
    format!(
        "{direction} {:.0}% from {} W.",
        delta.abs(),
        result.previous_ftp
    )
}

/// Where the number came from, in the rider's own terms.
fn evidence_sentence(result: &RampResult) -> String {
    let at = WorkoutEngine::format_duration(result.best_minute_start_secs);
    match result.steps_held {
        0 => format!("Your best minute started at {at}. Three quarters of it is your FTP."),
        1 => format!("One step held, best minute from {at}. Three quarters of it is your FTP."),
        n => format!("{n} steps held, best minute from {at}. Three quarters of it is your FTP."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(new_ftp: u32, previous_ftp: u32, steps_held: u32) -> RampResult {
        RampResult {
            new_ftp,
            previous_ftp,
            best_minute_watts: new_ftp * 4 / 3,
            best_minute_start_secs: 1_234,
            steps_held,
        }
    }

    #[test]
    fn should_name_the_direction_a_result_moves_the_number() {
        assert!(change_sentence(&result(240, 200, 9)).starts_with("Up 20% from 200 W"));
        assert!(change_sentence(&result(180, 200, 9)).starts_with("Down 10% from 200 W"));
    }

    #[test]
    fn should_say_the_number_is_right_rather_than_report_a_change_of_zero() {
        // "Up 0% from 200 W" is a sentence that means nothing to a rider.
        let sentence = change_sentence(&result(200, 200, 9));
        assert!(!sentence.contains('%'), "{sentence}");
        assert!(sentence.contains("is right"), "{sentence}");
    }

    #[test]
    fn should_not_print_a_negative_percentage_twice() {
        // The sign is carried by the word, so the number must be absolute —
        // "Down -10%" reads as an increase.
        assert!(!change_sentence(&result(180, 200, 9)).contains('-'));
    }

    #[test]
    fn should_count_one_step_in_the_singular() {
        assert!(evidence_sentence(&result(240, 200, 1)).starts_with("One step held"));
        assert!(evidence_sentence(&result(240, 200, 2)).starts_with("2 steps held"));
    }

    #[test]
    fn should_not_claim_any_steps_for_a_test_abandoned_in_the_warm_up() {
        let sentence = evidence_sentence(&result(240, 200, 0));
        assert!(!sentence.contains("step"), "{sentence}");
        assert!(sentence.contains("20:34"), "{sentence}");
    }
}
