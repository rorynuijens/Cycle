use adw::prelude::*;
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, db, keystore};

/// The stored settings the preferences window is built around.
struct PreferenceSettings {
    erg_ramp_rate: f64,
    sim_difficulty: f64,
    sim_max_gradient: f64,
    icu_athlete_id: String,
    icu_upload: bool,
    icu_sync: bool,
}

impl Default for PreferenceSettings {
    /// What each setting means when it has never been set.
    fn default() -> Self {
        Self {
            erg_ramp_rate: 25.0,
            sim_difficulty: 100.0,
            sim_max_gradient: 20.0,
            icu_athlete_id: String::new(),
            icu_upload: false,
            icu_sync: false,
        }
    }
}

/// Read every setting the window shows, in one pass off the GTK main thread.
///
/// An unset key falls back to its default; a failed *read* does not, because
/// the two are not the same thing — see [`show`].
async fn load_settings(pool: &SqlitePool) -> anyhow::Result<PreferenceSettings> {
    let defaults = PreferenceSettings::default();
    Ok(PreferenceSettings {
        erg_ramp_rate: db::get_setting(pool, "training.erg_ramp_rate")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.erg_ramp_rate),
        sim_difficulty: db::get_setting(pool, "training.sim_difficulty")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.sim_difficulty),
        sim_max_gradient: db::get_setting(pool, "training.sim_max_gradient")
            .await?
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(defaults.sim_max_gradient),
        icu_athlete_id: db::get_setting(pool, "intervals.athlete_id")
            .await?
            .unwrap_or_default(),
        icu_upload: db::get_setting(pool, "intervals.upload")
            .await?
            .map(|v| v == "1")
            .unwrap_or(defaults.icu_upload),
        icu_sync: db::get_setting(pool, "intervals.sync")
            .await?
            .map(|v| v == "1")
            .unwrap_or(defaults.icu_sync),
    })
}

/// Create and present the modal preferences window.
///
/// Changes apply immediately — no Save button. `on_saved` is called whenever the athlete
/// profile changes; `on_erg_rate_changed` is called whenever the ERG ramp rate changes;
/// `on_sim_changed` is called with `(difficulty_percent, max_gradient_percent)` whenever
/// either SIM mode setting changes.
///
/// The stored settings are read before the window is built, off the GTK main
/// thread (CLAUDE.md §2.3). They used to be read with `block_on` partway through
/// construction, which stalls the GLib loop whenever SQLite is busy — and it is
/// busy every 30 s during a ride, when the session checkpoint writes.
///
/// If the read fails the window does not open. Opening it on defaults would show
/// the rider values that are not the ones saved, and a rider who "corrects" one
/// of them writes the wrong value over a good one.
pub fn show(
    parent: &adw::ApplicationWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_saved: impl Fn(AthleteProfile) + 'static,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
) {
    let parent = parent.clone();
    let pool_load = pool.clone();
    let rt_build = rt_handle.clone();
    crate::ui::spawn_to_main(
        &rt_handle,
        async move { load_settings(&pool_load).await },
        move |result| match result {
            Ok(settings) => build_and_present(
                &parent,
                athlete,
                pool,
                rt_build,
                settings,
                on_saved,
                on_erg_rate_changed,
                on_sim_changed,
            ),
            Err(e) => {
                tracing::error!("Could not read your settings: {e}");
                let dialog = adw::AlertDialog::builder()
                    .heading("Could not open Preferences")
                    .body(
                        "Your settings could not be read. Preferences was not opened \
                         so that nothing overwrites them.",
                    )
                    .build();
                dialog.add_response("ok", "_OK");
                dialog.set_default_response(Some("ok"));
                dialog.present(Some(&parent));
            }
        },
    );
}

#[allow(clippy::too_many_arguments)] // preferences window wiring
fn build_and_present(
    parent: &adw::ApplicationWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    settings: PreferenceSettings,
    on_saved: impl Fn(AthleteProfile) + 'static,
    on_erg_rate_changed: impl Fn(u32) + 'static,
    on_sim_changed: impl Fn(u32, u32) + 'static,
) {
    let win = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .search_enabled(false)
        .title("Preferences")
        .build();

    // ── Page 1: Athlete ───────────────────────────────────────────────────
    let athlete_page = adw::PreferencesPage::builder()
        .title("Athlete")
        .icon_name("avatar-default-symbolic")
        .build();

    // ── Identity ──────────────────────────────────────────────────────────
    let identity_group = adw::PreferencesGroup::builder().title("Identity").build();
    let name_row = adw::EntryRow::builder().title("Name").build();
    name_row.set_text(&athlete.name);
    identity_group.add(&name_row);
    athlete_page.add(&identity_group);

    // ── Performance ───────────────────────────────────────────────────────
    let perf_group = adw::PreferencesGroup::builder()
        .title("Performance")
        .build();

    let ftp_adj = gtk::Adjustment::new(athlete.ftp_watts as f64, 50.0, 2000.0, 1.0, 10.0, 0.0);
    let ftp_row = adw::SpinRow::new(Some(&ftp_adj), 1.0, 0);
    ftp_row.set_title("FTP");
    ftp_row.set_subtitle("Functional Threshold Power (watts)");
    perf_group.add(&ftp_row);

    let weight_adj = gtk::Adjustment::new(athlete.weight_kg as f64, 30.0, 200.0, 0.5, 5.0, 0.0);
    let weight_row = adw::SpinRow::new(Some(&weight_adj), 1.0, 1);
    weight_row.set_title("Weight");
    weight_row.set_subtitle("Body weight (kg)");
    perf_group.add(&weight_row);
    athlete_page.add(&perf_group);

    // ── Heart Rate ────────────────────────────────────────────────────────
    let hr_group = adw::PreferencesGroup::builder().title("Heart Rate").build();

    let max_hr_adj = gtk::Adjustment::new(athlete.max_hr as f64, 100.0, 250.0, 1.0, 5.0, 0.0);
    let max_hr_row = adw::SpinRow::new(Some(&max_hr_adj), 1.0, 0);
    max_hr_row.set_title("Maximum HR");
    max_hr_row.set_subtitle("Maximum heart rate (bpm)");
    hr_group.add(&max_hr_row);

    let resting_hr_adj =
        gtk::Adjustment::new(athlete.resting_hr as f64, 30.0, 120.0, 1.0, 5.0, 0.0);
    let resting_hr_row = adw::SpinRow::new(Some(&resting_hr_adj), 1.0, 0);
    resting_hr_row.set_title("Resting HR");
    resting_hr_row.set_subtitle("Resting heart rate (bpm)");
    hr_group.add(&resting_hr_row);
    athlete_page.add(&hr_group);

    // ── Coaching context (moved here from the Coaching page) ──────────────
    let coaching_group = adw::PreferencesGroup::builder()
        .title("Coaching")
        .description("Context the AI Coach reads with every request.")
        .build();

    let context_row = adw::ActionRow::builder()
        .title("Training Context")
        .subtitle("Loading…")
        .use_markup(false)
        .activatable(true)
        .tooltip_text(
            "Describe your age, lifestyle, time constraints, and training preferences — \
             the more detail, the more personalised the coaching",
        )
        .build();
    context_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    coaching_group.add(&context_row);
    athlete_page.add(&coaching_group);

    // ── Danger Zone — widgets built here, signal wired after on_saved is wrapped ──
    let danger_group = adw::PreferencesGroup::builder()
        .title("Danger Zone")
        .description("These actions are permanent and cannot be undone.")
        .build();

    let delete_row = adw::ActionRow::builder()
        .title("Delete Athlete Profile")
        .subtitle("Reset your profile to defaults. API keys are not affected.")
        .build();

    let delete_btn = gtk::Button::builder()
        .label("Delete…")
        .css_classes(["destructive-action", "pill"])
        .tooltip_text("Permanently delete the athlete profile")
        .valign(gtk::Align::Center)
        .build();
    delete_row.add_suffix(&delete_btn);
    danger_group.add(&delete_row);
    athlete_page.add(&danger_group);

    win.add(&athlete_page);

    // ── Live-apply: each athlete change takes effect immediately ──────────
    let on_saved = Rc::new(on_saved);
    let athlete_id = athlete.id;
    let orig_name = athlete.name.clone();

    let apply: Rc<dyn Fn()> = {
        let name_row = name_row.clone();
        let ftp_row = ftp_row.clone();
        let weight_row = weight_row.clone();
        let max_hr_row = max_hr_row.clone();
        let resting_hr_row = resting_hr_row.clone();
        let on_saved = Rc::clone(&on_saved);
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();

        Rc::new(move || {
            let raw_name = name_row.text().to_string();
            let new_max_hr = max_hr_row.value() as u32;
            let new_resting_hr = (resting_hr_row.value() as u32).min(new_max_hr.saturating_sub(10));

            let new_athlete = AthleteProfile {
                id: athlete_id,
                name: if raw_name.trim().is_empty() {
                    orig_name.clone()
                } else {
                    raw_name.trim().to_string()
                },
                ftp_watts: ftp_row.value() as u32,
                weight_kg: weight_row.value() as f32,
                max_hr: new_max_hr,
                resting_hr: new_resting_hr,
            };

            on_saved(new_athlete.clone());

            let pool = pool.clone();
            rt_handle.spawn(async move {
                if let Err(e) = db::update_athlete(&pool, &new_athlete).await {
                    tracing::error!("update_athlete failed: {e}");
                }
            });
        })
    };

    {
        let a = Rc::clone(&apply);
        name_row.connect_apply(move |_| a());
    }
    {
        let a = Rc::clone(&apply);
        // Log manual FTP changes to ftp_history for FTP detection
        // (docs/ftp-detection.md) — debounced so spinner clicks produce one
        // entry with the final value, not one per notch.
        let log_generation = Rc::new(Cell::new(0u32));
        let last_logged_ftp = Rc::new(Cell::new(athlete.ftp_watts));
        let pool_log = pool.clone();
        let rt_log = rt_handle.clone();
        ftp_row.connect_value_notify(move |row| {
            a();
            let generation = log_generation.get().wrapping_add(1);
            log_generation.set(generation);
            let row = row.clone();
            let log_generation = Rc::clone(&log_generation);
            let last_logged_ftp = Rc::clone(&last_logged_ftp);
            let pool = pool_log.clone();
            let rt = rt_log.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                if log_generation.get() != generation {
                    return; // superseded by a newer change
                }
                let ftp = row.value() as u32;
                if ftp == last_logged_ftp.get() {
                    return;
                }
                last_logged_ftp.set(ftp);
                rt.spawn(async move {
                    if let Err(e) = db::log_ftp_change(&pool, ftp, "manual", "").await {
                        tracing::error!("log_ftp_change failed: {e}");
                    }
                });
            });
        });
    }
    {
        let a = Rc::clone(&apply);
        weight_row.connect_value_notify(move |_| a());
    }
    {
        let a = Rc::clone(&apply);
        max_hr_row.connect_value_notify(move |_| a());
    }
    {
        let a = Rc::clone(&apply);
        resting_hr_row.connect_value_notify(move |_| a());
    }

    // ── Delete profile confirmation dialog ────────────────────────────────
    {
        let pool_d = pool.clone();
        let rt_d = rt_handle.clone();
        let on_saved_d = Rc::clone(&on_saved);
        let win_d = win.clone();

        delete_btn.connect_clicked(move |btn| {
            let dialog = adw::AlertDialog::builder()
                .heading("Delete Athlete Profile?")
                .body(
                    "Your athlete profile (name, FTP, weight, heart rate) will be reset \
                     to defaults. This cannot be undone.\n\n\
                     API keys and device settings are always preserved.",
                )
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("delete", "_Delete");
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let check = gtk::CheckButton::builder()
                .label(
                    "Also delete all training data\n\
                     (sessions, workouts, calendar, activities, wellness, goals, time off)",
                )
                .active(false)
                .margin_top(6)
                .build();
            dialog.set_extra_child(Some(&check));

            let pool_c = pool_d.clone();
            let rt_c = rt_d.clone();
            let on_saved_c = Rc::clone(&on_saved_d);
            let win_c = win_d.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp != "delete" {
                    return;
                }
                let wipe_data = check.is_active();
                let pool_c = pool_c.clone();
                let on_saved_c = Rc::clone(&on_saved_c);
                let win_c = win_c.clone();
                crate::ui::spawn_to_main(
                    &rt_c,
                    async move {
                        if let Err(e) = db::reset_athlete_data(&pool_c, wipe_data).await {
                            tracing::error!("reset_athlete_data failed: {e}");
                            return Err(());
                        }
                        // Recreate a default athlete so the live engine remains valid
                        Ok(db::load_or_create_athlete(&pool_c)
                            .await
                            .unwrap_or_default())
                    },
                    move |res| match res {
                        Err(()) => {
                            win_c.add_toast(
                                adw::Toast::builder()
                                    .title("Failed to delete profile")
                                    .timeout(4)
                                    .build(),
                            );
                        }
                        Ok(default_athlete) => {
                            on_saved_c(default_athlete);
                            win_c.add_toast(
                                adw::Toast::builder()
                                    .title(if wipe_data {
                                        "Profile and all training data deleted"
                                    } else {
                                        "Athlete profile reset to defaults"
                                    })
                                    .timeout(4)
                                    .build(),
                            );
                            win_c.close();
                        }
                    },
                );
            });

            dialog.present(Some(btn));
        });
    }

    // ── Page 2: Training ──────────────────────────────────────────────────
    let training_page = adw::PreferencesPage::builder()
        .title("Training")
        .icon_name("media-playback-start-symbolic")
        .build();

    let erg_group = adw::PreferencesGroup::builder()
        .title("ERG Mode")
        .description(
            "Controls how quickly the trainer adjusts to a new power target. \
             Lower values feel smoother; set to 0 for an instant step change.",
        )
        .build();

    let ramp_adj = gtk::Adjustment::new(settings.erg_ramp_rate, 0.0, 100.0, 1.0, 5.0, 0.0);
    let ramp_row = adw::SpinRow::new(Some(&ramp_adj), 1.0, 0);
    ramp_row.set_title("Ramp Rate");
    ramp_row.set_subtitle("Watts per second (0 = instant)");
    erg_group.add(&ramp_row);
    training_page.add(&erg_group);

    // ── SIM mode (route rides) ────────────────────────────────────────────
    let sim_group = adw::PreferencesGroup::builder()
        .title("SIM Mode")
        .description(
            "How road gradients from a GPX route reach the trainer. \
             Lower the difficulty if steep climbs force you out of gears.",
        )
        .build();

    let difficulty_adj = gtk::Adjustment::new(settings.sim_difficulty, 0.0, 100.0, 5.0, 10.0, 0.0);
    let difficulty_row = adw::SpinRow::new(Some(&difficulty_adj), 5.0, 0);
    difficulty_row.set_title("Trainer Difficulty");
    difficulty_row.set_subtitle("Percentage of the real gradient sent to the trainer");
    sim_group.add(&difficulty_row);

    let max_grade_adj = gtk::Adjustment::new(settings.sim_max_gradient, 5.0, 20.0, 1.0, 5.0, 0.0);
    let max_grade_row = adw::SpinRow::new(Some(&max_grade_adj), 1.0, 0);
    max_grade_row.set_title("Maximum Gradient");
    max_grade_row.set_subtitle("Climbs steeper than this are capped (%)");
    sim_group.add(&max_grade_row);

    training_page.add(&sim_group);
    win.add(&training_page);

    {
        // Both rows report the pair, so the ride loop takes a single update.
        let on_sim_changed: Rc<dyn Fn(u32, u32)> = Rc::new(on_sim_changed);
        let pool_s = pool.clone();
        let rt_s = rt_handle.clone();

        let save_sim = move |difficulty: u32, max_grade: u32| {
            let pool = pool_s.clone();
            rt_s.spawn(async move {
                if let Err(e) =
                    db::set_setting(&pool, "training.sim_difficulty", &difficulty.to_string()).await
                {
                    tracing::error!("save training.sim_difficulty failed: {e}");
                }
                if let Err(e) =
                    db::set_setting(&pool, "training.sim_max_gradient", &max_grade.to_string())
                        .await
                {
                    tracing::error!("save training.sim_max_gradient failed: {e}");
                }
            });
        };
        let save_sim: Rc<dyn Fn(u32, u32)> = Rc::new(save_sim);

        {
            let cb = Rc::clone(&on_sim_changed);
            let save = Rc::clone(&save_sim);
            let other = max_grade_row.clone();
            difficulty_row.connect_value_notify(move |row| {
                let (d, m) = (row.value() as u32, other.value() as u32);
                cb(d, m);
                save(d, m);
            });
        }
        {
            let cb = Rc::clone(&on_sim_changed);
            let save = Rc::clone(&save_sim);
            let other = difficulty_row.clone();
            max_grade_row.connect_value_notify(move |row| {
                let (d, m) = (other.value() as u32, row.value() as u32);
                cb(d, m);
                save(d, m);
            });
        }
    }

    {
        let pool_r = pool.clone();
        let rt_r = rt_handle.clone();
        ramp_row.connect_value_notify(move |row| {
            let rate = row.value() as u32;
            on_erg_rate_changed(rate);
            let pool = pool_r.clone();
            rt_r.spawn(async move {
                if let Err(e) =
                    db::set_setting(&pool, "training.erg_ramp_rate", &rate.to_string()).await
                {
                    tracing::error!("save training.erg_ramp_rate failed: {e}");
                }
            });
        });
    }

    // ── Page 3: Integrations ──────────────────────────────────────────────
    let integrations_page = adw::PreferencesPage::builder()
        .title("Integrations")
        .icon_name("share-symbolic")
        .build();

    // ── Intervals.icu ─────────────────────────────────────────────────────
    let icu_athlete_id = settings.icu_athlete_id.clone();
    // Keyring reads are fast local D-Bus calls, not database or network work.
    let icu_api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
        .unwrap_or(None)
        .unwrap_or_default();
    let icu_upload = settings.icu_upload;
    let icu_sync = settings.icu_sync;

    let icu_group = adw::PreferencesGroup::builder()
        .title("Intervals.icu")
        .description(
            "Your Intervals.icu API key is stored locally and sent only to \
             intervals.icu when syncing. Find your key at intervals.icu → Settings → API.",
        )
        .build();

    // Athlete ID entry row
    let icu_id_row = adw::EntryRow::builder()
        .title("Athlete ID")
        .show_apply_button(true)
        .build();
    icu_id_row.set_text(&icu_athlete_id);
    icu_group.add(&icu_id_row);

    // API key view/edit/remove (same UX as Anthropic key)
    let icu_has_key = !icu_api_key.trim().is_empty();
    let icu_stored_key: Rc<RefCell<String>> = Rc::new(RefCell::new(icu_api_key.clone()));

    let icu_status_row = adw::ActionRow::builder()
        .title("API Key")
        .subtitle(key_subtitle(&icu_api_key))
        .visible(icu_has_key)
        .build();

    let icu_edit_btn = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Edit API key")
        .valign(gtk::Align::Center)
        .build();
    let icu_remove_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .css_classes(["destructive-action", "flat", "circular"])
        .tooltip_text("Remove API key")
        .valign(gtk::Align::Center)
        .build();
    icu_status_row.add_suffix(&icu_edit_btn);
    icu_status_row.add_suffix(&icu_remove_btn);
    icu_group.add(&icu_status_row);

    let icu_entry_row = adw::PasswordEntryRow::builder()
        .title("API Key")
        .visible(!icu_has_key)
        .build();
    icu_entry_row.set_show_apply_button(true);

    let icu_cancel_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Cancel")
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    icu_entry_row.add_suffix(&icu_cancel_btn);
    icu_group.add(&icu_entry_row);

    // Upload sessions toggle
    let icu_upload_row = adw::SwitchRow::builder()
        .title("Upload sessions")
        .subtitle("Automatically upload completed workouts to Intervals.icu")
        .active(icu_upload)
        .build();
    icu_group.add(&icu_upload_row);

    // Sync activities toggle
    let icu_sync_row = adw::SwitchRow::builder()
        .title("Sync activities")
        .subtitle("Include Intervals.icu activities in training load (CTL/ATL/TSB)")
        .active(icu_sync)
        .build();
    icu_group.add(&icu_sync_row);

    // Sync now row
    let sync_action_row = adw::ActionRow::builder()
        .title("Activity Sync")
        .subtitle("Download the last 90 days of activities from Intervals.icu")
        .build();
    let sync_spinner = gtk::Spinner::new();
    sync_spinner.set_visible(false);
    sync_spinner.set_valign(gtk::Align::Center);
    let sync_now_btn = gtk::Button::builder()
        .label("Sync Now")
        .css_classes(["pill"])
        .tooltip_text("Download recent activities from Intervals.icu into training load")
        .valign(gtk::Align::Center)
        .build();
    sync_action_row.add_suffix(&sync_spinner);
    sync_action_row.add_suffix(&sync_now_btn);
    icu_group.add(&sync_action_row);

    // Workout library sync (moved here from the Coaching page) — the AI Coach
    // offers synced workouts alongside the built-in library in suggestions
    let lib_row = adw::ActionRow::builder()
        .title("Workout Library")
        .subtitle("Loading…")
        .build();
    let lib_spinner = gtk::Spinner::new();
    lib_spinner.set_visible(false);
    lib_spinner.set_valign(gtk::Align::Center);
    let lib_sync_btn = gtk::Button::builder()
        .label("Sync Library")
        .css_classes(["pill"])
        .tooltip_text("Download your Intervals.icu workout library for AI suggestions")
        .valign(gtk::Align::Center)
        .build();
    lib_row.add_suffix(&lib_spinner);
    lib_row.add_suffix(&lib_sync_btn);
    icu_group.add(&lib_row);

    integrations_page.add(&icu_group);

    // ── AI Coaching ───────────────────────────────────────────────────────
    let ai_group = adw::PreferencesGroup::builder()
        .title("AI Coaching")
        .description(
            "Your AI provider API key is stored locally on this device and only sent to \
             your AI provider when you request a coaching suggestion or fitness analysis.",
        )
        .build();

    let current_key = keystore::get_secret(keystore::KEY_ANTHROPIC)
        .unwrap_or(None)
        .unwrap_or_default();
    let has_key = !current_key.trim().is_empty();

    // Keeps the current key value in sync across all callbacks
    let stored_key: Rc<RefCell<String>> = Rc::new(RefCell::new(current_key.clone()));

    // View row — shown when a key is already configured
    let status_row = adw::ActionRow::builder()
        .title("AI Provider API Key")
        .subtitle(key_subtitle(&current_key))
        .visible(has_key)
        .build();

    let edit_btn = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Edit API key")
        .valign(gtk::Align::Center)
        .build();
    let remove_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .css_classes(["destructive-action", "flat", "circular"])
        .tooltip_text("Remove API key")
        .valign(gtk::Align::Center)
        .build();
    status_row.add_suffix(&edit_btn);
    status_row.add_suffix(&remove_btn);
    ai_group.add(&status_row);

    // Entry row — shown when entering a new key or editing the existing one
    let entry_row = adw::PasswordEntryRow::builder()
        .title("AI Provider API Key")
        .visible(!has_key)
        .build();
    entry_row.set_show_apply_button(true);

    // Cancel button — only visible when editing an existing key (not first-time entry)
    let cancel_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Cancel")
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    entry_row.add_suffix(&cancel_btn);
    ai_group.add(&entry_row);

    integrations_page.add(&ai_group);

    win.add(&integrations_page);

    // ── Intervals.icu athlete ID ──────────────────────────────────────────
    {
        let pool_id = pool.clone();
        let rt_id = rt_handle.clone();
        icu_id_row.connect_apply(move |row| {
            let value = row.text().trim().to_string();
            let pool = pool_id.clone();
            rt_id.spawn(async move {
                if let Err(e) = db::set_setting(&pool, "intervals.athlete_id", &value).await {
                    tracing::error!("save intervals.athlete_id failed: {e}");
                }
            });
        });
    }

    // ── Coaching: training context preview + editor ───────────────────────
    {
        // Populate the preview subtitle off the main thread (CLAUDE.md §2.3)
        let row_c = context_row.clone();
        let pool_c = pool.clone();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move {
                db::get_setting(&pool_c, "coaching.athlete_context")
                    .await
                    .unwrap_or(None)
                    .unwrap_or_default()
            },
            move |ctx| row_c.set_subtitle(&context_preview(&ctx)),
        );

        let pool_e = pool.clone();
        let rt_e = rt_handle.clone();
        let win_e = win.clone();
        context_row.connect_activated(move |row| {
            show_context_editor(&win_e, pool_e.clone(), rt_e.clone(), row.clone());
        });
    }

    // ── Intervals.icu workout library: count + sync ───────────────────────
    {
        let row_l = lib_row.clone();
        let pool_l = pool.clone();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move { db::count_intervals_workouts(&pool_l).await.unwrap_or(0) },
            move |count| row_l.set_subtitle(&library_subtitle(count)),
        );

        let pool_sl = pool.clone();
        let rt_sl = rt_handle.clone();
        let spinner_sl = lib_spinner.clone();
        let row_sl = lib_row.clone();
        let win_sl = win.clone();
        lib_sync_btn.connect_clicked(move |btn| {
            let api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
                .unwrap_or(None)
                .unwrap_or_default();

            // Load the athlete ID off the main thread (CLAUDE.md §2.3); the
            // credential check, spinner, and network sync follow on arrival.
            let pool_load = pool_sl.clone();
            let pool_net = pool_sl.clone();
            let rt_net = rt_sl.clone();
            let spinner_sl = spinner_sl.clone();
            let row_sl = row_sl.clone();
            let win_sl = win_sl.clone();
            let btn = btn.clone();

            crate::ui::spawn_to_main(
                &rt_sl,
                async move {
                    db::get_setting(&pool_load, "intervals.athlete_id")
                        .await
                        .unwrap_or(None)
                        .unwrap_or_default()
                },
                move |athlete_id| {
                    if api_key.trim().is_empty() || athlete_id.trim().is_empty() {
                        win_sl.add_toast(
                            adw::Toast::builder()
                                .title("Set your Intervals.icu API key and Athlete ID above first")
                                .timeout(5)
                                .build(),
                        );
                        return;
                    }

                    btn.set_sensitive(false);
                    spinner_sl.set_visible(true);
                    spinner_sl.start();

                    let pool_async = pool_net.clone();
                    let (tx, rx) = async_channel::bounded::<Result<usize, String>>(1);
                    rt_net.spawn(async move {
                        let result =
                            match crate::ai::intervals::fetch_workouts(&athlete_id, &api_key).await
                            {
                                Ok(workouts) => {
                                    match db::clear_intervals_workouts(&pool_async).await {
                                        Err(e) => Err(e.to_string()),
                                        Ok(_) => {
                                            let count = workouts.len();
                                            for w in workouts {
                                                if let Err(e) = db::upsert_intervals_workout(
                                                    &pool_async,
                                                    &w.id,
                                                    &w.name,
                                                    &w.description,
                                                    w.target_duration,
                                                    w.icu_training_load,
                                                )
                                                .await
                                                {
                                                    tracing::error!(
                                                        "upsert intervals workout: {e}"
                                                    );
                                                }
                                            }
                                            Ok(count)
                                        }
                                    }
                                }
                                Err(e) => Err(e.to_string()),
                            };
                        let _ = tx.send(result).await;
                    });

                    let btn_c = btn.clone();
                    let spinner_c = spinner_sl.clone();
                    let row_c = row_sl.clone();
                    let win_c = win_sl.clone();
                    glib::MainContext::default().spawn_local(async move {
                        if let Ok(result) = rx.recv().await {
                            match result {
                                Ok(count) => {
                                    row_c.set_subtitle(&library_subtitle(count as i64));
                                    win_c.add_toast(
                                        adw::Toast::builder()
                                            .title("Workout library synced")
                                            .timeout(3)
                                            .build(),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("Intervals.icu library sync failed: {e}");
                                    win_c.add_toast(
                                        adw::Toast::builder()
                                            .title("Library sync failed — check your credentials")
                                            .timeout(5)
                                            .build(),
                                    );
                                }
                            }
                        }
                        spinner_c.stop();
                        spinner_c.set_visible(false);
                        btn_c.set_sensitive(true);
                    });
                },
            );
        });
    }

    // ── Intervals.icu API key: edit/cancel/remove/apply ───────────────────
    {
        let icu_status_c = icu_status_row.clone();
        let icu_entry_c = icu_entry_row.clone();
        let icu_cancel_c = icu_cancel_btn.clone();
        let icu_stored_c = Rc::clone(&icu_stored_key);
        icu_edit_btn.connect_clicked(move |_| {
            icu_entry_c.set_text(&icu_stored_c.borrow());
            icu_status_c.set_visible(false);
            icu_entry_c.set_visible(true);
            icu_cancel_c.set_visible(true);
        });
    }
    {
        let icu_status_c = icu_status_row.clone();
        let icu_entry_c = icu_entry_row.clone();
        icu_cancel_btn.connect_clicked(move |btn| {
            icu_entry_c.set_visible(false);
            icu_status_c.set_visible(true);
            btn.set_visible(false);
        });
    }
    {
        let icu_status_r = icu_status_row.clone();
        let icu_entry_r = icu_entry_row.clone();
        let icu_cancel_r = icu_cancel_btn.clone();
        let icu_stored_r = Rc::clone(&icu_stored_key);
        let win_r = win.clone();
        icu_remove_btn.connect_clicked(move |_| {
            *icu_stored_r.borrow_mut() = String::new();
            icu_entry_r.set_text("");
            icu_status_r.set_visible(false);
            icu_entry_r.set_visible(true);
            icu_cancel_r.set_visible(false);
            win_r.add_toast(
                adw::Toast::builder()
                    .title("API key removed")
                    .timeout(3)
                    .build(),
            );
            if let Err(e) = keystore::delete_secret(keystore::KEY_INTERVALS_API) {
                tracing::error!("clear intervals.api_key failed: {e}");
            }
        });
    }
    {
        let icu_status_a = icu_status_row.clone();
        let icu_entry_a = icu_entry_row.clone();
        let icu_cancel_a = icu_cancel_btn.clone();
        let icu_stored_a = Rc::clone(&icu_stored_key);
        let win_a = win.clone();
        icu_entry_row.connect_apply(move |row| {
            let trimmed = row.text().trim().to_string();
            if trimmed.is_empty() {
                return;
            }
            *icu_stored_a.borrow_mut() = trimmed.clone();
            icu_status_a.set_subtitle(&key_subtitle(&trimmed));
            icu_status_a.set_visible(true);
            icu_entry_a.set_visible(false);
            icu_cancel_a.set_visible(false);
            win_a.add_toast(
                adw::Toast::builder()
                    .title("API key saved")
                    .timeout(3)
                    .build(),
            );
            if let Err(e) = keystore::set_secret(keystore::KEY_INTERVALS_API, &trimmed) {
                tracing::error!("save intervals.api_key failed: {e}");
            } else {
                tracing::debug!("Intervals.icu API key saved (not logged)");
            }
        });
    }

    // ── Intervals.icu toggle switches ─────────────────────────────────────
    {
        let pool = pool.clone();
        let rt = rt_handle.clone();
        icu_upload_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            let pool = pool.clone();
            rt.spawn(async move {
                if let Err(e) =
                    db::set_setting(&pool, "intervals.upload", if enabled { "1" } else { "0" })
                        .await
                {
                    tracing::error!("save intervals.upload failed: {e}");
                }
            });
        });
    }
    {
        let pool = pool.clone();
        let rt = rt_handle.clone();
        icu_sync_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            let pool = pool.clone();
            rt.spawn(async move {
                if let Err(e) =
                    db::set_setting(&pool, "intervals.sync", if enabled { "1" } else { "0" }).await
                {
                    tracing::error!("save intervals.sync failed: {e}");
                }
            });
        });
    }

    // ── Sync Activities Now ───────────────────────────────────────────────
    {
        let pool_s = pool.clone();
        let rt_s = rt_handle.clone();
        let spinner_s = sync_spinner.clone();
        let win_s = win.clone();
        let id_row_s = icu_id_row.clone();

        sync_now_btn.connect_clicked(move |btn| {
            // Read the ID off the row rather than back out of the database: it is
            // already on screen, it saves a blocking read on the GTK thread, and
            // an ID just typed works without having to commit the row first.
            let athlete_id = id_row_s.text().trim().to_string();
            let api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
                .unwrap_or(None)
                .unwrap_or_default();

            if athlete_id.trim().is_empty() || api_key.trim().is_empty() {
                win_s.add_toast(
                    adw::Toast::builder()
                        .title("Set your Athlete ID and API key first")
                        .timeout(3)
                        .build(),
                );
                return;
            }

            btn.set_sensitive(false);
            spinner_s.set_visible(true);
            spinner_s.start();

            let pool_async = pool_s.clone();
            let (tx, rx) = async_channel::bounded::<Result<(usize, usize), String>>(1);
            rt_s.spawn(async move {
                let newest = chrono::Local::now().date_naive();
                let oldest = newest - chrono::Duration::days(90);

                // Sync activities
                let act_count = match crate::ai::intervals::fetch_activities(
                    &athlete_id,
                    &api_key,
                    oldest,
                    newest,
                )
                .await
                {
                    Ok(activities) => {
                        let mut count = 0usize;
                        for a in activities {
                            match crate::data::db::upsert_intervals_activity(
                                &pool_async,
                                &a.id,
                                a.start_date_local,
                                &a.name,
                                a.icu_training_load,
                                a.moving_time,
                                a.average_watts,
                                a.normalized_watts,
                                a.average_hr,
                                a.max_hr,
                                &a.sport_type,
                                a.start_datetime_local,
                                a.distance_m,
                                a.elevation_gain_m,
                                a.average_cadence,
                            )
                            .await
                            {
                                Ok(_) => count += 1,
                                Err(e) => tracing::error!("upsert intervals activity: {e}"),
                            }
                        }
                        // A ride recorded in-app can arrive back here after a round
                        // trip through Garmin or Strava — link the two so it is shown
                        // and counted once.
                        if let Err(e) = crate::data::db::reconcile_icu_links(&pool_async).await {
                            tracing::error!("reconcile_icu_links: {e}");
                        }
                        count
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string())).await;
                        return;
                    }
                };

                // Sync wellness (last 30 days)
                let wellness_oldest = newest - chrono::Duration::days(30);
                let wellness_count = match crate::ai::intervals::fetch_wellness(
                    &athlete_id,
                    &api_key,
                    wellness_oldest,
                    newest,
                )
                .await
                {
                    Ok(entries) => {
                        let mut count = 0usize;
                        for e in entries {
                            let db_entry = crate::data::db::WellnessEntry {
                                date: e.date,
                                hrv: e.hrv,
                                resting_hr: e.resting_hr,
                                sleep_secs: e.sleep_secs,
                                sleep_score: e.sleep_score,
                                steps: e.steps,
                                calories: e.calories,
                            };
                            match crate::data::db::upsert_wellness_entry(&pool_async, &db_entry)
                                .await
                            {
                                Ok(_) => count += 1,
                                Err(e) => tracing::error!("upsert wellness entry: {e}"),
                            }
                        }
                        count
                    }
                    Err(e) => {
                        tracing::warn!("Wellness sync failed (non-fatal): {e}");
                        0
                    }
                };

                let _ = tx.send(Ok((act_count, wellness_count))).await;
            });

            let btn_c = btn.clone();
            let spinner_c = spinner_s.clone();
            let win_c = win_s.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    match result {
                        Ok((act_count, well_count)) => {
                            let msg = if well_count > 0 {
                                format!(
                                    "Synced {act_count} activities and {well_count} wellness \
                                     entries from Intervals.icu"
                                )
                            } else {
                                format!("Synced {act_count} activities from Intervals.icu")
                            };
                            win_c.add_toast(adw::Toast::builder().title(msg).timeout(4).build());
                        }
                        Err(e) => {
                            win_c.add_toast(
                                adw::Toast::builder()
                                    .title(format!("Sync failed: {e}"))
                                    .timeout(5)
                                    .build(),
                            );
                        }
                    }
                }
                spinner_c.stop();
                spinner_c.set_visible(false);
                btn_c.set_sensitive(true);
            });
        });
    }

    // Edit button — pre-fill entry and switch to edit mode
    {
        let status_row_c = status_row.clone();
        let entry_row_c = entry_row.clone();
        let cancel_btn_c = cancel_btn.clone();
        let stored_key_c = Rc::clone(&stored_key);
        edit_btn.connect_clicked(move |_| {
            entry_row_c.set_text(&stored_key_c.borrow());
            status_row_c.set_visible(false);
            entry_row_c.set_visible(true);
            cancel_btn_c.set_visible(true);
        });
    }

    // Cancel button — revert to view mode without saving
    {
        let status_row_c = status_row.clone();
        let entry_row_c = entry_row.clone();
        cancel_btn.connect_clicked(move |btn| {
            entry_row_c.set_visible(false);
            status_row_c.set_visible(true);
            btn.set_visible(false);
        });
    }

    // Remove button — clear the stored key and return to entry mode
    {
        let status_row_r = status_row.clone();
        let entry_row_r = entry_row.clone();
        let cancel_btn_r = cancel_btn.clone();
        let stored_key_r = Rc::clone(&stored_key);
        let win_r = win.clone();
        remove_btn.connect_clicked(move |_| {
            *stored_key_r.borrow_mut() = String::new();
            entry_row_r.set_text("");
            status_row_r.set_visible(false);
            entry_row_r.set_visible(true);
            cancel_btn_r.set_visible(false);
            win_r.add_toast(
                adw::Toast::builder()
                    .title("API key removed")
                    .timeout(3)
                    .build(),
            );
            if let Err(e) = keystore::delete_secret(keystore::KEY_ANTHROPIC) {
                tracing::error!("clear anthropic.api_key failed: {e}");
            }
        });
    }

    // Apply — save the key, show confirmation toast, switch to view mode
    {
        let status_row_a = status_row.clone();
        let entry_row_a = entry_row.clone();
        let cancel_btn_a = cancel_btn.clone();
        let stored_key_a = Rc::clone(&stored_key);
        let win_a = win.clone();
        entry_row.connect_apply(move |row| {
            let trimmed = row.text().trim().to_string();
            if trimmed.is_empty() {
                return;
            }
            *stored_key_a.borrow_mut() = trimmed.clone();
            status_row_a.set_subtitle(&key_subtitle(&trimmed));
            status_row_a.set_visible(true);
            entry_row_a.set_visible(false);
            cancel_btn_a.set_visible(false);
            win_a.add_toast(
                adw::Toast::builder()
                    .title("API key saved")
                    .timeout(3)
                    .build(),
            );
            if let Err(e) = keystore::set_secret(keystore::KEY_ANTHROPIC, &trimmed) {
                tracing::error!("save anthropic.api_key failed: {e}");
            } else {
                tracing::debug!("AI provider API key saved (not logged)");
            }
        });
    }

    win.present();
}

fn key_subtitle(key: &str) -> String {
    let t = key.trim();
    if t.len() >= 8 {
        format!("Configured · ends in ···{}", &t[t.len() - 4..])
    } else if !t.is_empty() {
        "Configured".to_string()
    } else {
        "Not configured".to_string()
    }
}

/// First line of the training context, truncated, for the preferences row subtitle.
fn context_preview(ctx: &str) -> String {
    let first = ctx.trim().lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "Not set — the AI Coach gives better advice with context".to_string();
    }
    let truncated: String = first.chars().take(72).collect();
    if truncated.len() < first.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Subtitle for the Intervals.icu workout library row.
fn library_subtitle(count: i64) -> String {
    if count > 0 {
        format!("{count} workouts synced — offered to the AI Coach in suggestions")
    } else {
        "Not synced yet — sync to include your library in AI suggestions".to_string()
    }
}

/// Present the training-context editor dialog (moved here from the Coaching page).
/// Saves to `coaching.athlete_context` and refreshes `preview_row`'s subtitle.
fn show_context_editor(
    win: &adw::PreferencesWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    preview_row: adw::ActionRow,
) {
    let win = win.clone();
    let pool_load = pool.clone();
    let rt_save = rt_handle.clone();
    // Load the current context off the main thread (CLAUDE.md §2.3), then
    // build and present the editor dialog when it arrives.
    crate::ui::spawn_to_main(
        &rt_handle,
        async move {
            db::get_setting(&pool_load, "coaching.athlete_context")
                .await
                .unwrap_or(None)
                .unwrap_or_default()
        },
        move |current| {
            let content_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .margin_top(12)
                .margin_bottom(24)
                .margin_start(24)
                .margin_end(24)
                .build();

            content_box.append(
                &gtk::Label::builder()
                    .label(
                        "Describe your age, lifestyle, time constraints, and training \
                         preferences. The AI Coach uses this in every coaching response.",
                    )
                    .css_classes(["dim-label"])
                    .halign(gtk::Align::Start)
                    .wrap(true)
                    .build(),
            );

            let template_btn = gtk::Button::builder()
                .label("Use template")
                .css_classes(["pill"])
                .tooltip_text("Fill in a starter template")
                .halign(gtk::Align::Start)
                .build();
            content_box.append(&template_btn);

            let text_view = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::Word)
                .accepts_tab(false)
                .hexpand(true)
                .build();
            text_view.buffer().set_text(&current);

            let tv_scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .min_content_height(120)
                .hexpand(true)
                .build();
            tv_scroll.set_child(Some(&text_view));

            let tv_frame = gtk::Box::builder()
                .css_classes(["card"])
                .orientation(gtk::Orientation::Vertical)
                .build();
            tv_frame.append(&tv_scroll);
            content_box.append(&tv_frame);

            {
                let tv = text_view.clone();
                template_btn.connect_clicked(move |_| {
                    tv.buffer().set_text(
                        "I am [AGE] years old [GENDER]. [DESCRIBE YOUR LIFESTYLE AND \
                         TIME CONSTRAINTS].\nMy training goals are: [LIST YOUR GOALS].\n\
                         I prefer workouts that are [PREFERENCES — e.g. time-efficient, \
                         varied, low-impact].\nAdditional notes: [ANYTHING ELSE].",
                    );
                });
            }

            let toolbar_view = adw::ToolbarView::new();
            let header = adw::HeaderBar::new();

            let cancel_btn = gtk::Button::builder()
                .label("Cancel")
                .tooltip_text("Discard changes")
                .build();
            let save_btn = gtk::Button::builder()
                .label("Save")
                .css_classes(["suggested-action"])
                .tooltip_text("Save training context")
                .build();
            header.pack_start(&cancel_btn);
            header.pack_end(&save_btn);
            toolbar_view.add_top_bar(&header);
            toolbar_view.set_content(Some(&content_box));

            let dialog = adw::Dialog::builder()
                .title("Training Context")
                .child(&toolbar_view)
                .content_width(560)
                .build();

            let dialog_cancel = dialog.clone();
            cancel_btn.connect_clicked(move |_| {
                dialog_cancel.close();
            });

            let dialog_save = dialog.clone();
            let win_save = win.clone();
            save_btn.connect_clicked(move |_| {
                let buf = text_view.buffer();
                let text = buf
                    .text(&buf.start_iter(), &buf.end_iter(), false)
                    .to_string();
                let trimmed = text.trim().to_string();
                let pool = pool.clone();
                let ctx = trimmed.clone();
                rt_save.spawn(async move {
                    if let Err(e) = db::set_setting(&pool, "coaching.athlete_context", &ctx).await {
                        tracing::error!("save coaching.athlete_context failed: {e}");
                    }
                });
                preview_row.set_subtitle(&context_preview(&trimmed));
                win_save.add_toast(
                    adw::Toast::builder()
                        .title("Training context saved")
                        .timeout(3)
                        .build(),
                );
                dialog_save.close();
            });

            dialog.present(Some(&win));
        },
    );
}
