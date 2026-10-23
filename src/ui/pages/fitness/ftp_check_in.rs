//! The FTP check-in: what the last four weeks of threshold work say about the
//! rider's FTP.
//!
//! Accept-only, like the ramp test's card (`ui/widgets/ftp_result.rs`): FTP is
//! the number every target, zone and load figure is scaled to, so it moves when
//! the rider presses the button and not before. The verdict is reached in
//! [`crate::training::ftp_detect::check_in`]; this only says what it found.

use adw::prelude::*;
use chrono::NaiveDate;
use std::rc::Rc;

use crate::training::ftp_detect::{Direction, FtpSuggestion, Snooze};

/// Said before the rider accepts: nothing has changed yet.
const OFFERED: &str = "From your threshold work over the last four weeks. Nothing has \
                       changed yet — accepting sets your FTP and records it in your history.";
/// Said once they have.
const ACCEPTED: &str = "Your FTP is updated, and this check-in is in your FTP history.";
/// Said over a verdict that leaves the number where it is.
const HOLDING: &str = "From your threshold work over the last four weeks.";

pub struct FtpCheckIn {
    holder: gtk::Box,
    /// Handed the new FTP; it is what actually changes anything.
    on_accept: Rc<dyn Fn(u32)>,
    /// Handed the snooze to store when the rider sets the card aside.
    on_snooze: Rc<dyn Fn(Snooze)>,
}

impl FtpCheckIn {
    pub fn new(on_accept: Rc<dyn Fn(u32)>, on_snooze: Rc<dyn Fn(Snooze)>) -> Self {
        let holder = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .visible(false)
            .build();
        Self {
            holder,
            on_accept,
            on_snooze,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.holder
    }

    /// Show `suggestion`, or hide the card when there is none. `today` dates
    /// the snooze if the rider sets the card aside.
    pub fn set(&self, suggestion: Option<FtpSuggestion>, current_ftp: u32, today: NaiveDate) {
        while let Some(child) = self.holder.first_child() {
            self.holder.remove(&child);
        }
        let Some(suggestion) = suggestion else {
            self.holder.set_visible(false);
            return;
        };
        self.holder.set_visible(true);
        self.holder.append(&build(
            &suggestion,
            current_ftp,
            today,
            &self.holder,
            Rc::clone(&self.on_accept),
            Rc::clone(&self.on_snooze),
        ));
    }
}

fn build(
    suggestion: &FtpSuggestion,
    current_ftp: u32,
    today: NaiveDate,
    holder: &gtk::Box,
    on_accept: Rc<dyn Fn(u32)>,
    on_snooze: Rc<dyn Fn(Snooze)>,
) -> adw::PreferencesGroup {
    let moves = offers_change(suggestion.direction);
    let group = adw::PreferencesGroup::builder()
        .title("FTP Check-in")
        .description(if moves { OFFERED } else { HOLDING })
        .build();

    group.add(
        &adw::ActionRow::builder()
            .title(headline(suggestion))
            .subtitle(change_sentence(suggestion, current_ftp))
            .title_lines(0)
            .subtitle_lines(0)
            .use_markup(false)
            .build(),
    );
    for line in &suggestion.evidence {
        group.add(
            &adw::ActionRow::builder()
                .title(line.as_str())
                .title_lines(0)
                .use_markup(false)
                .css_classes(["property"])
                .build(),
        );
    }

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .valign(gtk::Align::Center)
        .build();

    // "Not now" on a change, "Got it" on a hold: either way the card goes for
    // two weeks, and comes back sooner only if the verdict turns.
    let snooze_btn = gtk::Button::builder()
        .label(snooze_label(suggestion.direction))
        .css_classes(["pill"])
        .tooltip_text("Hide the check-in for two weeks")
        .build();
    let direction = suggestion.direction;
    let on_snooze_accept = Rc::clone(&on_snooze);
    snooze_btn.connect_clicked(glib::clone!(
        // Weak: the holder owns this button through the group.
        #[weak]
        holder,
        move |_| {
            on_snooze(Snooze::starting(today, direction));
            holder.set_visible(false);
        }
    ));
    buttons.append(&snooze_btn);

    if moves {
        let new_ftp = suggestion.new_ftp;
        let accept_btn = gtk::Button::builder()
            .label(format!("Set FTP to {new_ftp} W"))
            .css_classes(["suggested-action", "pill"])
            .tooltip_text("Use this as your FTP from now on")
            .build();
        // Updated in place rather than rebuilt: the handler runs on a widget
        // this group owns (CLAUDE.md §2.4).
        accept_btn.connect_clicked(glib::clone!(
            #[weak]
            group,
            #[weak]
            buttons,
            move |_| {
                buttons.set_visible(false);
                group.set_description(Some(ACCEPTED));
                on_accept(new_ftp);
                // The change starts the cooldown, and a card saying "looks
                // right" the next time the page loads would tell the rider
                // nothing. A drop still comes through: it points the other way.
                on_snooze_accept(Snooze::starting(today, Direction::Hold));
            }
        ));
        buttons.append(&accept_btn);
    }
    group.set_header_suffix(Some(&buttons));
    group
}

/// Whether the card offers to set a new FTP. A hold offers only "Got it": a
/// button that sets FTP to the number it already is would do nothing.
fn offers_change(direction: Direction) -> bool {
    direction != Direction::Hold
}

/// "Not now" on a change, "Got it" on a hold.
fn snooze_label(direction: Direction) -> &'static str {
    if offers_change(direction) {
        "Not now"
    } else {
        "Got it"
    }
}

/// The number, and whether it is a change.
fn headline(suggestion: &FtpSuggestion) -> String {
    match suggestion.direction {
        Direction::Hold => format!("FTP {} W looks right", suggestion.new_ftp),
        Direction::Up | Direction::Down => format!("FTP {} W", suggestion.new_ftp),
    }
}

fn change_sentence(suggestion: &FtpSuggestion, current_ftp: u32) -> String {
    match suggestion.direction {
        Direction::Hold => "No change suggested.".to_string(),
        Direction::Up => format!(
            "Up {}% from {current_ftp} W.",
            percent(suggestion.delta_pct)
        ),
        Direction::Down => format!(
            "Down {}% from {current_ftp} W.",
            percent(suggestion.delta_pct)
        ),
    }
}

/// A change as the rider reads it: "3", or "4.5" when it is not whole. Never
/// signed — the sentence says which way.
fn percent(delta_pct: f32) -> String {
    let tenths = (delta_pct.abs() * 10.0).round() as u32;
    if tenths.is_multiple_of(10) {
        format!("{}", tenths / 10)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion(new_ftp: u32, delta_pct: f32, direction: Direction) -> FtpSuggestion {
        FtpSuggestion {
            new_ftp,
            delta_pct,
            direction,
            evidence: vec![],
        }
    }

    #[test]
    fn should_offer_a_new_ftp_only_when_the_number_moves() {
        assert!(offers_change(Direction::Up));
        assert!(offers_change(Direction::Down));
        assert!(!offers_change(Direction::Hold));
        assert_eq!(snooze_label(Direction::Up), "Not now");
        assert_eq!(snooze_label(Direction::Down), "Not now");
        assert_eq!(snooze_label(Direction::Hold), "Got it");
    }

    #[test]
    fn should_write_a_whole_change_without_a_decimal() {
        assert_eq!(percent(3.0), "3");
        assert_eq!(percent(-5.0), "5");
        assert_eq!(percent(0.0), "0");
    }

    #[test]
    fn should_keep_the_half_of_a_four_and_a_half_percent_drop() {
        // 200 → 191 W is −4.5 %. "{:.0}" rounds half to even and would say 4.
        assert_eq!(percent(-4.5), "4.5");
        assert_eq!(
            change_sentence(&suggestion(191, -4.5, Direction::Down), 200),
            "Down 4.5% from 200 W."
        );
    }

    #[test]
    fn should_round_float_noise_out_of_the_percentage() {
        // 206 W from 200 W is 3.0000038 % in f32.
        let delta = (206.0_f32 - 200.0) / 200.0 * 100.0;
        assert_eq!(percent(delta), "3");
        assert_eq!(percent(2.96), "3");
        assert_eq!(percent(2.94), "2.9");
    }

    #[test]
    fn should_say_which_way_the_number_moves() {
        assert_eq!(
            change_sentence(&suggestion(206, 3.0, Direction::Up), 200),
            "Up 3% from 200 W."
        );
        assert_eq!(headline(&suggestion(206, 3.0, Direction::Up)), "FTP 206 W");
    }

    #[test]
    fn should_say_the_number_is_right_on_a_hold() {
        let hold = suggestion(200, 0.0, Direction::Hold);
        assert_eq!(headline(&hold), "FTP 200 W looks right");
        assert!(!change_sentence(&hold, 200).contains('%'));
    }
}
