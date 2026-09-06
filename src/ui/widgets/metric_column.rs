//! A centred caption-over-value column, as used by every live cockpit.
//!
//! Shared by the workout player, the route player and the ride overlay. It was
//! written twice before the overlay needed a third copy.

use adw::prelude::*;

/// Character widths reserved for the cockpit numbers, so the layout stops
/// depending on the value — see [`metric_column`].
///
/// Power is four because a sprint reads four digits and the trainer is trusted
/// no further (CLAUDE.md §5.1 clamps it well below 10 000 W); a clock is six
/// because a long ride passes `120:00`; heart rate and cadence are three
/// because both are clamped at 250.
pub const POWER_DIGITS: i32 = 4;
pub const CLOCK_DIGITS: i32 = 6;
pub const RATE_DIGITS: i32 = 3;

/// A centred caption-over-value column for a cockpit metric row.
///
/// `unit` goes in the caption and `digits` reserves the value's width, because
/// on these pages a label that asks for exactly the room its text needs moves
/// the whole window. The hero row is `column_homogeneous`, so every column is
/// as wide as the widest — one more digit on the power number costs *three*
/// columns of width, and at 620 % type that measured as a jump in the window's
/// minimum width from 777 px to 951 px at three digits and 1128 px at four. The
/// rider sees the cockpit lurch sideways the moment they push over 100 W.
///
/// So the width is reserved for the widest value the field can hold and never
/// changes again, and the unit moves to the caption: " W" is two characters of
/// the reservation, which at hero size is about 100 px spent on a letter that
/// never changes. `Power (W)` says it once instead.
///
/// The overlay ([`crate::ui::overlay`]) depends on this more sharply than
/// either full page does: it is a few hundred pixels wide and sits over video,
/// where a window that resizes itself every time the rider pushes over 100 W is
/// unusable rather than merely untidy.
///
/// Returns the column and the value label, so the caller can keep the label and
/// update it each tick.
pub fn metric_column(
    title: &str,
    unit: Option<&str>,
    initial: &str,
    value_css: &[&str],
    digits: i32,
) -> (gtk::Box, gtk::Label) {
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .build();

    vbox.append(
        &gtk::Label::builder()
            .label(match unit {
                Some(u) => format!("{title} ({u})"),
                None => title.to_string(),
            })
            .css_classes(["caption", "dim-label"])
            .build(),
    );

    let value_label = gtk::Label::builder()
        .label(initial)
        .width_chars(digits)
        .css_classes(value_css.to_vec())
        .build();

    vbox.append(&value_label);
    (vbox, value_label)
}
