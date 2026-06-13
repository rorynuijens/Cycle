use adw::prelude::*;
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::{athlete::AthleteProfile, db, keystore};

/// Create and present the modal preferences window.
///
/// Changes apply immediately — no Save button. `on_saved` is called whenever the athlete
/// profile changes; `on_erg_rate_changed` is called whenever the ERG ramp rate changes.
pub fn show(
    parent: &adw::ApplicationWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_saved: impl Fn(AthleteProfile) + 'static,
    on_erg_rate_changed: impl Fn(u32) + 'static,
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
        ftp_row.connect_value_notify(move |_| a());
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

    let saved_ramp_rate = rt_handle
        .block_on(db::get_setting(&pool, "training.erg_ramp_rate"))
        .unwrap_or(None)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(25.0);

    let ramp_adj = gtk::Adjustment::new(saved_ramp_rate, 0.0, 100.0, 1.0, 5.0, 0.0);
    let ramp_row = adw::SpinRow::new(Some(&ramp_adj), 1.0, 0);
    ramp_row.set_title("Ramp Rate");
    ramp_row.set_subtitle("Watts per second (0 = instant)");
    erg_group.add(&ramp_row);
    training_page.add(&erg_group);
    win.add(&training_page);

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
        .icon_name("emblem-shared-symbolic")
        .build();

    // ── Intervals.icu ─────────────────────────────────────────────────────
    let icu_athlete_id = rt_handle
        .block_on(db::get_setting(&pool, "intervals.athlete_id"))
        .unwrap_or(None)
        .unwrap_or_default();
    let icu_api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
        .unwrap_or(None)
        .unwrap_or_default();
    let icu_upload = rt_handle
        .block_on(db::get_setting(&pool, "intervals.upload"))
        .unwrap_or(None)
        .map(|v| v == "1")
        .unwrap_or(false);
    let icu_sync = rt_handle
        .block_on(db::get_setting(&pool, "intervals.sync"))
        .unwrap_or(None)
        .map(|v| v == "1")
        .unwrap_or(false);

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

        sync_now_btn.connect_clicked(move |btn| {
            let athlete_id = rt_s
                .block_on(db::get_setting(&pool_s, "intervals.athlete_id"))
                .unwrap_or(None)
                .unwrap_or_default();
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
