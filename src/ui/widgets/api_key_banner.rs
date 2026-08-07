//! The banner telling a rider that the app's AI features need an API key.
//!
//! Shown on every page that offers one, so the check and the way out of it live
//! in one place.

use adw::prelude::*;

use crate::data::keystore;

/// A banner that reveals itself whenever no API key is stored.
#[derive(Clone)]
pub struct ApiKeyBanner {
    banner: adw::Banner,
}

impl ApiKeyBanner {
    /// Build the banner, hidden until [`Self::refresh`] finds no key.
    pub fn new(title: &str) -> Self {
        let banner = adw::Banner::builder()
            .title(title)
            .button_label("Open Preferences")
            .revealed(false)
            .build();

        // The banner only ever shows to someone with no key yet, so its button
        // has to take them somewhere. `app.preferences` is the same action the
        // main menu uses.
        banner.connect_button_clicked(|banner| {
            if let Err(e) = banner.activate_action("app.preferences", None) {
                tracing::error!("Could not open Preferences from the API key banner: {e}");
            }
        });

        Self { banner }
    }

    pub fn widget(&self) -> &adw::Banner {
        &self.banner
    }

    /// Show or hide the banner according to whether a key is stored.
    ///
    /// Reads the local keyring, which is fast enough to stay on the main thread
    /// — unlike the database work the pages do around it. A keyring that cannot
    /// be read is treated as having no key: the rider is better off being told
    /// to check Preferences than being left to wonder why the AI never answers.
    pub fn refresh(&self) {
        let has_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
            .unwrap_or(None)
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);
        self.banner.set_revealed(!has_key);
    }
}
