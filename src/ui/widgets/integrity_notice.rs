//! The notice that says a ride's numbers are not being counted, and why.
//!
//! Shown in two places — the summary the rider sees when they get off the bike,
//! and the ride's detail view later — so that what was found and what follows
//! from it are worded once. See [`crate::training::integrity`] for the checks
//! themselves.

use adw::prelude::*;
use std::rc::Rc;

use crate::data::session::Session;
use crate::training::integrity::{self, Verdict};

/// What being flagged actually costs the rider. Said in full, because a ride
/// quietly missing from the Fitness chart is the thing this exists to prevent.
const WITHHELD: &str =
    "Not counted towards your fitness, and not shown to the coach, until you say otherwise.";

/// Said once the rider has decided they know better.
const COUNTED: &str = "You chose to count this ride. Its numbers are used as they stand.";

/// Fill `holder` with a notice about `session`, or leave it empty and hidden
/// when the ride has nothing wrong with it.
///
/// `on_count_anyway` is called when the rider asks for the ride to count; the
/// caller stores that decision. The notice updates itself either way, so a
/// caller with nowhere to reload from still shows the right thing.
pub fn attach(holder: &gtk::Box, session: &Session, on_count_anyway: Rc<dyn Fn()>) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }

    let verdict = integrity::check(session);
    if verdict.is_trusted() {
        holder.set_visible(false);
        return;
    }
    holder.set_visible(true);
    holder.append(&build(
        &verdict,
        session.integrity_dismissed,
        on_count_anyway,
    ));
}

fn build(
    verdict: &Verdict,
    dismissed: bool,
    on_count_anyway: Rc<dyn Fn()>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Check this ride")
        .description(if dismissed { COUNTED } else { WITHHELD })
        .build();

    for concern in verdict.concerns() {
        let row = adw::ActionRow::builder()
            .title(concern.describe())
            .subtitle(concern.remedy())
            // Both strings are sentences written for a rider, and a narrow
            // window must wrap them rather than trim them to an ellipsis.
            .title_lines(0)
            .subtitle_lines(0)
            // They are plain text, not markup: an ampersand in one must not be
            // read as the start of an entity.
            .use_markup(false)
            .build();

        let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
        icon.add_css_class("warning");
        // The icon carries the severity, so a screen reader has to be told what
        // it means — there is no other text saying this is a warning.
        icon.update_property(&[gtk::accessible::Property::Label("Warning")]);
        row.add_prefix(&icon);
        group.add(&row);
    }

    if !dismissed {
        let button = gtk::Button::builder()
            .label("Count it anyway")
            .valign(gtk::Align::Center)
            .tooltip_text("Count this ride towards your fitness and show it to the coach")
            .build();

        // Updated in place rather than rebuilt: the handler is running on a
        // widget this group owns, and tearing that widget down underneath it is
        // how re-entrancy bugs start.
        button.connect_clicked(glib::clone!(
            // Strong would be a cycle: the group owns this button through its
            // header suffix, and the button's handler would own the group back.
            #[weak]
            group,
            move |btn| {
                btn.set_visible(false);
                group.set_description(Some(COUNTED));
                on_count_anyway();
            }
        ));
        group.set_header_suffix(Some(&button));
    }

    group
}
