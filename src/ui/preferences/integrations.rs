//! The Integrations page: Intervals.icu and the AI provider.
//!
//! Both API keys go to the GNOME keyring rather than the database (CLAUDE.md
//! §5.2), through the shared [`ApiKeyRow`].

use adw::prelude::*;
use chrono::{Duration, Local};
use gtk::glib;
use sqlx::SqlitePool;

use crate::data::settings::{self, IntervalsSettings};
use crate::data::{db, keystore};
use crate::ui::spawn_write;
use crate::ui::widgets::api_key_row::ApiKeyRow;

/// How far back "Sync Now" pulls activities.
const ACTIVITY_SYNC_DAYS: i64 = 90;

/// How far back "Sync Now" pulls wellness entries. Shorter than the activity
/// window because only recent wellness informs current form.
const WELLNESS_SYNC_DAYS: i64 = 30;

/// Subtitle for the Intervals.icu workout library row.
fn library_subtitle(count: i64) -> String {
    if count > 0 {
        format!("{count} workouts synced — offered to the AI Coach in suggestions")
    } else {
        "Not synced yet — sync to include your library in AI suggestions".to_string()
    }
}

/// What a finished activity sync has to report.
fn sync_summary(activities: usize, wellness: usize) -> String {
    if wellness > 0 {
        format!("Synced {activities} activities and {wellness} wellness entries from Intervals.icu")
    } else {
        format!("Synced {activities} activities from Intervals.icu")
    }
}

/// Build the Integrations page.
pub fn build(
    win: &adw::PreferencesWindow,
    intervals: &IntervalsSettings,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Integrations")
        .icon_name("share-symbolic")
        .build();

    // ── Intervals.icu ─────────────────────────────────────────────────────
    let icu_group = adw::PreferencesGroup::builder()
        .title("Intervals.icu")
        .description(
            "Your Intervals.icu API key is stored locally and sent only to \
             intervals.icu when syncing. Find your key at intervals.icu → Settings → API.",
        )
        .build();

    let id_row = adw::EntryRow::builder()
        .title("Athlete ID")
        .show_apply_button(true)
        .build();
    id_row.set_text(&intervals.athlete_id);
    icu_group.add(&id_row);

    let icu_key = ApiKeyRow::new(win, "API Key", keystore::KEY_INTERVALS_API);
    icu_key.add_to(&icu_group);

    let upload_row = adw::SwitchRow::builder()
        .title("Upload sessions")
        .subtitle("Automatically upload completed workouts to Intervals.icu")
        .active(intervals.upload)
        .build();
    icu_group.add(&upload_row);

    let sync_row = adw::SwitchRow::builder()
        .title("Sync activities")
        .subtitle("Include Intervals.icu activities in training load (CTL/ATL/TSB)")
        .active(intervals.sync)
        .build();
    icu_group.add(&sync_row);

    let sync_action_row = adw::ActionRow::builder()
        .title("Activity Sync")
        .subtitle(format!(
            "Download the last {ACTIVITY_SYNC_DAYS} days of activities from Intervals.icu"
        ))
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

    // The AI Coach offers synced workouts alongside the built-in library.
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

    page.add(&icu_group);

    // ── AI Coaching ───────────────────────────────────────────────────────
    let ai_group = adw::PreferencesGroup::builder()
        .title("AI Coaching")
        .description(
            "Your AI provider API key is stored locally on this device and only sent to \
             your AI provider when you request a coaching suggestion or fitness analysis.",
        )
        .build();
    let ai_key = ApiKeyRow::new(win, "AI Provider API Key", keystore::KEY_ANTHROPIC);
    ai_key.add_to(&ai_group);
    page.add(&ai_group);

    // ── Wiring ────────────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        id_row.connect_apply(move |row| {
            let value = row.text().trim().to_string();
            spawn_write(
                &rt_handle,
                &pool,
                "the Intervals.icu athlete ID",
                |pool| async move { settings::set_intervals_athlete_id(&pool, &value).await },
            );
        });
    }
    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        upload_row.connect_active_notify(move |row| {
            let on = row.is_active();
            spawn_write(
                &rt_handle,
                &pool,
                "the upload setting",
                move |pool| async move { settings::set_intervals_upload(&pool, on).await },
            );
        });
    }
    {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        sync_row.connect_active_notify(move |row| {
            let on = row.is_active();
            spawn_write(
                &rt_handle,
                &pool,
                "the sync setting",
                move |pool| async move { settings::set_intervals_sync(&pool, on).await },
            );
        });
    }

    connect_library(
        &lib_row,
        &lib_sync_btn,
        &lib_spinner,
        win,
        &pool,
        &rt_handle,
    );
    connect_activity_sync(
        &sync_now_btn,
        &sync_spinner,
        &id_row,
        win,
        &pool,
        &rt_handle,
    );

    page
}

/// Show the stored library count, and sync it on demand.
fn connect_library(
    lib_row: &adw::ActionRow,
    button: &gtk::Button,
    spinner: &gtk::Spinner,
    win: &adw::PreferencesWindow,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    // Count off the main thread (CLAUDE.md §2.3).
    let row = lib_row.clone();
    let pool_count = pool.clone();
    crate::ui::spawn_to_main(
        rt_handle,
        async move { db::count_intervals_workouts(&pool_count).await.unwrap_or(0) },
        move |count| row.set_subtitle(&library_subtitle(count)),
    );

    let pool = pool.clone();
    let rt_handle = rt_handle.clone();
    let spinner = spinner.clone();
    let lib_row = lib_row.clone();
    let win = win.clone();

    button.connect_clicked(move |btn| {
        let api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
            .unwrap_or(None)
            .unwrap_or_default();

        // Read the athlete ID off the main thread (CLAUDE.md §2.3); the
        // credential check, spinner and sync all follow on arrival.
        let pool_id = pool.clone();
        let pool_sync = pool.clone();
        let rt_sync = rt_handle.clone();
        let spinner = spinner.clone();
        let lib_row = lib_row.clone();
        let win = win.clone();
        let btn = btn.clone();

        crate::ui::spawn_to_main(
            &rt_handle,
            async move {
                settings::load_intervals(&pool_id)
                    .await
                    .map(|s| s.athlete_id)
                    .unwrap_or_default()
            },
            move |athlete_id| {
                if api_key.trim().is_empty() || athlete_id.trim().is_empty() {
                    win.add_toast(
                        adw::Toast::builder()
                            .title("Set your Intervals.icu API key and Athlete ID above first")
                            .timeout(5)
                            .build(),
                    );
                    return;
                }

                btn.set_sensitive(false);
                spinner.set_visible(true);
                spinner.start();

                let (tx, rx) = async_channel::bounded::<Result<usize, String>>(1);
                let pool_task = pool_sync.clone();
                rt_sync.spawn(async move {
                    let result = sync_workout_library(&pool_task, &athlete_id, &api_key).await;
                    let _ = tx.send(result).await;
                });

                let btn = btn.clone();
                let spinner = spinner.clone();
                let lib_row = lib_row.clone();
                let win = win.clone();
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(result) = rx.recv().await {
                        match result {
                            Ok(count) => {
                                lib_row.set_subtitle(&library_subtitle(count as i64));
                                win.add_toast(
                                    adw::Toast::builder()
                                        .title("Workout library synced")
                                        .timeout(3)
                                        .build(),
                                );
                            }
                            Err(e) => {
                                tracing::error!("Intervals.icu library sync failed: {e}");
                                win.add_toast(
                                    adw::Toast::builder()
                                        .title("Library sync failed — check your credentials")
                                        .timeout(5)
                                        .build(),
                                );
                            }
                        }
                    }
                    spinner.stop();
                    spinner.set_visible(false);
                    btn.set_sensitive(true);
                });
            },
        );
    });
}

/// Replace the stored workout library with what Intervals.icu currently has.
async fn sync_workout_library(
    pool: &SqlitePool,
    athlete_id: &str,
    api_key: &str,
) -> Result<usize, String> {
    let workouts = crate::ai::intervals::fetch_workouts(athlete_id, api_key)
        .await
        .map_err(|e| e.to_string())?;
    // Cleared first, so a workout deleted upstream stops being offered here.
    db::clear_intervals_workouts(pool)
        .await
        .map_err(|e| e.to_string())?;

    let count = workouts.len();
    for w in workouts {
        if let Err(e) = db::upsert_intervals_workout(
            pool,
            &w.id,
            &w.name,
            &w.description,
            w.target_duration,
            w.icu_training_load,
        )
        .await
        {
            tracing::error!("upsert intervals workout: {e}");
        }
    }
    Ok(count)
}

/// Download recent activities and wellness on demand.
fn connect_activity_sync(
    button: &gtk::Button,
    spinner: &gtk::Spinner,
    id_row: &adw::EntryRow,
    win: &adw::PreferencesWindow,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    let pool = pool.clone();
    let rt_handle = rt_handle.clone();
    let spinner = spinner.clone();
    let id_row = id_row.clone();
    let win = win.clone();

    button.connect_clicked(move |btn| {
        // Read the ID off the row rather than back out of the database: it is
        // already on screen, it saves a blocking read on the GTK thread, and an
        // ID just typed works without having to commit the row first.
        let athlete_id = id_row.text().trim().to_string();
        let api_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
            .unwrap_or(None)
            .unwrap_or_default();

        if athlete_id.is_empty() || api_key.trim().is_empty() {
            win.add_toast(
                adw::Toast::builder()
                    .title("Set your Athlete ID and API key first")
                    .timeout(3)
                    .build(),
            );
            return;
        }

        btn.set_sensitive(false);
        spinner.set_visible(true);
        spinner.start();

        let (tx, rx) = async_channel::bounded::<Result<(usize, usize), String>>(1);
        let pool_task = pool.clone();
        rt_handle.spawn(async move {
            let _ = tx
                .send(sync_activities(&pool_task, &athlete_id, &api_key).await)
                .await;
        });

        let btn = btn.clone();
        let spinner = spinner.clone();
        let win = win.clone();
        glib::MainContext::default().spawn_local(async move {
            if let Ok(result) = rx.recv().await {
                let toast = match result {
                    Ok((activities, wellness)) => adw::Toast::builder()
                        .title(sync_summary(activities, wellness))
                        .timeout(4)
                        .build(),
                    Err(e) => adw::Toast::builder()
                        .title(format!("Sync failed: {e}"))
                        .timeout(5)
                        .build(),
                };
                win.add_toast(toast);
            }
            spinner.stop();
            spinner.set_visible(false);
            btn.set_sensitive(true);
        });
    });
}

/// Pull activities and wellness, returning how many of each were stored.
///
/// A wellness failure is not fatal: the activities are the point, and wellness
/// is only ever supporting detail.
async fn sync_activities(
    pool: &SqlitePool,
    athlete_id: &str,
    api_key: &str,
) -> Result<(usize, usize), String> {
    let newest = Local::now().date_naive();

    let activities = crate::ai::intervals::fetch_activities(
        athlete_id,
        api_key,
        newest - Duration::days(ACTIVITY_SYNC_DAYS),
        newest,
    )
    .await
    .map_err(|e| e.to_string())?;

    let mut activity_count = 0usize;
    for a in activities {
        match db::upsert_intervals_activity(
            pool,
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
            Ok(_) => activity_count += 1,
            Err(e) => tracing::error!("upsert intervals activity: {e}"),
        }
    }

    // A ride recorded in-app can arrive back here after a round trip through
    // Garmin or Strava — link the two so it is shown and counted once.
    if let Err(e) = db::reconcile_icu_links(pool).await {
        tracing::error!("reconcile_icu_links: {e}");
    }

    let wellness_count = match crate::ai::intervals::fetch_wellness(
        athlete_id,
        api_key,
        newest - Duration::days(WELLNESS_SYNC_DAYS),
        newest,
    )
    .await
    {
        Ok(entries) => {
            let mut count = 0usize;
            for e in entries {
                let entry = db::WellnessEntry {
                    date: e.date,
                    hrv: e.hrv,
                    resting_hr: e.resting_hr,
                    sleep_secs: e.sleep_secs,
                    sleep_score: e.sleep_score,
                    steps: e.steps,
                    calories: e.calories,
                };
                match db::upsert_wellness_entry(pool, &entry).await {
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

    Ok((activity_count, wellness_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_invite_a_first_library_sync() {
        assert!(library_subtitle(0).starts_with("Not synced yet"));
    }

    #[test]
    fn should_report_how_many_workouts_are_synced() {
        assert!(library_subtitle(42).starts_with("42 workouts synced"));
    }

    #[test]
    fn should_report_both_halves_of_a_sync() {
        let summary = sync_summary(12, 30);
        assert!(summary.contains("12 activities"), "got {summary}");
        assert!(summary.contains("30 wellness"), "got {summary}");
    }

    #[test]
    fn should_not_mention_wellness_when_none_came_back() {
        // Wellness is optional and its failure is non-fatal, so a zero means
        // "not available" rather than "you had none" — better left unsaid.
        let summary = sync_summary(12, 0);
        assert!(!summary.contains("wellness"), "got {summary}");
        assert!(summary.contains("12 activities"));
    }
}
