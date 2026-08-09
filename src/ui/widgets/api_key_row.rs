//! The rows that manage one stored API key.
//!
//! A key is either configured — shown as a status row with edit and remove
//! buttons — or not, in which case a password entry takes its place. The rows
//! swap as the rider moves between those states; the key itself never leaves
//! the keyring except to prefill the entry when editing.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::keystore;

/// How a stored key is described without showing it.
///
/// The tail is taken in *characters*, not bytes. The key is pasted by the
/// rider and nothing guarantees it is ASCII, so slicing four bytes off the end
/// could land inside a character and panic.
fn key_subtitle(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return "Not configured".to_string();
    }
    let chars = key.chars().count();
    if chars >= 8 {
        let tail: String = key.chars().skip(chars - 4).collect();
        format!("Configured · ends in ···{tail}")
    } else {
        // Too short to show a tail without giving away most of the key.
        "Configured".to_string()
    }
}

pub struct ApiKeyRow {
    status: adw::ActionRow,
    entry: adw::PasswordEntryRow,
    cancel: gtk::Button,
    /// The current key, so Edit can prefill the entry without going back to
    /// the keyring on every click.
    stored: Rc<RefCell<String>>,
}

impl ApiKeyRow {
    /// Build the rows for the key stored under `keystore_key`.
    ///
    /// `title` names the key in the UI; toasts go to `win`.
    pub fn new(win: &adw::PreferencesWindow, title: &str, keystore_key: &'static str) -> Self {
        // Keyring reads are fast local D-Bus calls, not database or network work.
        let current = keystore::get_secret(keystore_key)
            .unwrap_or(None)
            .unwrap_or_default();
        let configured = !current.trim().is_empty();

        let status = adw::ActionRow::builder()
            .title(title)
            .subtitle(key_subtitle(&current))
            .visible(configured)
            .build();

        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Edit API key")
            .valign(gtk::Align::Center)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["destructive-action", "flat", "circular"])
            .tooltip_text("Remove API key")
            .valign(gtk::Align::Center)
            .build();
        status.add_suffix(&edit);
        status.add_suffix(&remove);

        let entry = adw::PasswordEntryRow::builder()
            .title(title)
            .visible(!configured)
            .build();
        entry.set_show_apply_button(true);

        // Only offered when editing an existing key — there is nothing to
        // return to when entering one for the first time.
        let cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Cancel")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        entry.add_suffix(&cancel);

        let row = Self {
            status,
            entry,
            cancel,
            stored: Rc::new(RefCell::new(current)),
        };
        row.connect(&edit, &remove, win, keystore_key);
        row
    }

    /// Add both rows to a group, in display order.
    pub fn add_to(&self, group: &adw::PreferencesGroup) {
        group.add(&self.status);
        group.add(&self.entry);
    }

    fn connect(
        &self,
        edit: &gtk::Button,
        remove: &gtk::Button,
        win: &adw::PreferencesWindow,
        keystore_key: &'static str,
    ) {
        // Edit — prefill the entry and switch to edit mode.
        let (status, entry, cancel, stored) = self.handles();
        edit.connect_clicked(move |_| {
            entry.set_text(&stored.borrow());
            status.set_visible(false);
            entry.set_visible(true);
            cancel.set_visible(true);
        });

        // Cancel — back to view mode without saving.
        let (status, entry, _, _) = self.handles();
        self.cancel.connect_clicked(move |btn| {
            entry.set_visible(false);
            status.set_visible(true);
            btn.set_visible(false);
        });

        // Remove — clear the key and return to entry mode.
        let (status, entry, cancel, stored) = self.handles();
        // Weak: this row lives inside the preferences window (CLAUDE.md §2.4).
        remove.connect_clicked(glib::clone!(
            #[weak]
            win,
            move |_| {
                stored.borrow_mut().clear();
                entry.set_text("");
                status.set_visible(false);
                entry.set_visible(true);
                cancel.set_visible(false);
                win.add_toast(
                    adw::Toast::builder()
                        .title("API key removed")
                        .timeout(3)
                        .build(),
                );
                if let Err(e) = keystore::delete_secret(keystore_key) {
                    tracing::error!("Could not clear {keystore_key}: {e}");
                }
            }
        ));

        // Apply — store the key and switch to view mode.
        let (status, entry, cancel, stored) = self.handles();
        // Weak: this row lives inside the preferences window (CLAUDE.md §2.4).
        self.entry.connect_apply(glib::clone!(
            #[weak]
            win,
            move |row| {
                let key = row.text().trim().to_string();
                if key.is_empty() {
                    return;
                }
                status.set_subtitle(&key_subtitle(&key));
                status.set_visible(true);
                entry.set_visible(false);
                cancel.set_visible(false);
                win.add_toast(
                    adw::Toast::builder()
                        .title("API key saved")
                        .timeout(3)
                        .build(),
                );
                match keystore::set_secret(keystore_key, &key) {
                    // Never log the key itself (CLAUDE.md §5.6).
                    Ok(()) => tracing::debug!("{keystore_key} saved (not logged)"),
                    Err(e) => tracing::error!("Could not save {keystore_key}: {e}"),
                }
                *stored.borrow_mut() = key;
            }
        ));
    }

    /// Fresh handles on the same widgets, for moving into a callback.
    fn handles(
        &self,
    ) -> (
        adw::ActionRow,
        adw::PasswordEntryRow,
        gtk::Button,
        Rc<RefCell<String>>,
    ) {
        (
            self.status.clone(),
            self.entry.clone(),
            self.cancel.clone(),
            Rc::clone(&self.stored),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_say_nothing_is_configured_for_an_empty_key() {
        assert_eq!(key_subtitle(""), "Not configured");
        assert_eq!(key_subtitle("   "), "Not configured");
    }

    #[test]
    fn should_show_the_last_four_characters_of_a_long_key() {
        assert_eq!(
            key_subtitle("sk-ant-api03-abcd1234"),
            "Configured · ends in ···1234"
        );
    }

    #[test]
    fn should_hide_the_tail_of_a_short_key() {
        // Four of seven characters would give most of it away.
        assert_eq!(key_subtitle("abc1234"), "Configured");
    }

    #[test]
    fn should_show_a_tail_at_exactly_eight_characters() {
        assert_eq!(key_subtitle("abcd1234"), "Configured · ends in ···1234");
    }

    #[test]
    fn should_ignore_whitespace_around_a_pasted_key() {
        assert_eq!(
            key_subtitle("  sk-ant-api03-abcd1234\n"),
            "Configured · ends in ···1234"
        );
    }

    #[test]
    fn should_not_panic_on_a_key_ending_in_multibyte_characters() {
        // Nothing stops the rider pasting arbitrary text into the entry, and
        // taking the last four *bytes* would land inside a character here.
        // "anahtarım-şğüöç" is 15 characters but 20 bytes.
        assert_eq!(
            key_subtitle("anahtarım-şğüöç"),
            "Configured · ends in ···ğüöç"
        );
    }

    #[test]
    fn should_count_characters_not_bytes_when_deciding_to_show_a_tail() {
        // Five characters, but ten bytes — a byte-length test would wrongly
        // treat this as long enough to show a tail.
        assert_eq!(key_subtitle("şğüöç"), "Configured");
    }
}
