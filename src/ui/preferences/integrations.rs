//! The Integrations page: Garmin Connect, Intervals.icu and the AI provider.
//!
//! Garmin Connect is where activities, their files and wellness come from once
//! its key is set; Intervals.icu keeps the workout library and the optional
//! upload, and syncs activities only for a rider without a Garmin key. Every
//! API key goes to the GNOME keyring rather than the database (CLAUDE.md §5.2),
//! through the shared [`ApiKeyRow`].

use adw::prelude::*;
use chrono::{Duration, Local};
use gtk::glib;
use sqlx::SqlitePool;

use crate::ai::garmin::sync as garmin;
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
fn sync_summary(counts: SyncCounts) -> String {
    let SyncCounts {
        source,
        activities,
        withheld,
        merged,
        wellness,
    } = counts;
    let tail = format!(
        "{}{}",
        crate::ai::intervals::withheld_suffix(withheld),
        garmin::merged_suffix(merged)
    );
    if wellness > 0 {
        format!(
            "Synced {activities} activities and {wellness} wellness entries \
             from {source}{tail}"
        )
    } else {
        format!("Synced {activities} activities from {source}{tail}")
    }
}

/// What the Import History row says under its title while an import runs.
fn import_progress(p: garmin::Progress) -> String {
    match p {
        garmin::Progress::Wellness { done, total } => {
            format!("Wellness — {} %", percent(done, total))
        }
        garmin::Progress::Activities => "Listing activities…".to_string(),
        garmin::Progress::Files { done, total } => {
            format!("Activity files — {done} of {total}")
        }
    }
}

/// `done` of `total` as a whole percentage; nothing to do is all done.
fn percent(done: usize, total: usize) -> usize {
    (done.min(total) * 100).checked_div(total).unwrap_or(100)
}

/// The toast at the end of an import.
fn import_summary(r: &garmin::ImportReport) -> String {
    let mut text = format!(
        "Imported {} activities, {} activity files and {} days of wellness",
        r.activities, r.files.fetched, r.wellness
    );
    if r.files.unauthorized {
        text.push_str(" — Garmin refused the key part-way; import again with a new one to finish");
    } else if r.files.stopped {
        text.push_str(" — stopped early; import again to pick up where it left off");
    }
    text
}

/// What the Import History row says when no import is running.
const IMPORT_SUBTITLE: &str = "Every activity, activity file and day of wellness Garmin holds. \
     Years of history take about half an hour; an import that stops picks up where it left off.";

/// A Garmin failure, as the rider is told it.
fn garmin_failure(e: &anyhow::Error) -> String {
    if garmin::is_unauthorized(e) {
        garmin::UNAUTHORIZED_NOTICE.to_string()
    } else {
        format!("{e:#}")
    }
}

/// Read a key-expiry date as typed. `Ok(None)` is an empty row: no date.
fn parse_expiry(text: &str) -> Result<Option<chrono::NaiveDate>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| ())
}

/// Build the Integrations page.
pub fn build(
    win: &adw::PreferencesWindow,
    parent: &adw::ApplicationWindow,
    intervals: &IntervalsSettings,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Integrations")
        .icon_name("share-symbolic")
        .build();

    // ── Garmin Connect ────────────────────────────────────────────────────
    let garmin_group = adw::PreferencesGroup::builder()
        .title("Garmin Connect")
        .description(
            "Activities, their files and wellness come from Garmin Connect once a personal \
             API key is set; without one, activities sync from Intervals.icu. The key is \
             stored in the keyring and sent only to Garmin. Create one at Garmin Connect → \
             Settings → Personal API Key.",
        )
        .build();
    let garmin_key = ApiKeyRow::new(win, "Personal API Key", keystore::KEY_GARMIN_API);
    garmin_key.add_to(&garmin_group);

    let expiry_row = adw::EntryRow::builder()
        .title("Key Expires (YYYY-MM-DD)")
        .show_apply_button(true)
        .tooltip_text(
            "The expiry date Garmin Connect shows for the key — you are warned a week before",
        )
        .build();
    garmin_group.add(&expiry_row);

    let sync_row = adw::SwitchRow::builder()
        .title("Sync activities")
        .subtitle("Include synced activities in training load (CTL/ATL/TSB)")
        .active(intervals.sync)
        .build();
    garmin_group.add(&sync_row);

    let sync_action_row = adw::ActionRow::builder()
        .title("Activity Sync")
        .subtitle(format!(
            "Download the last {ACTIVITY_SYNC_DAYS} days of activities — from Intervals.icu \
             when no Garmin key is set"
        ))
        .build();
    let sync_spinner = gtk::Spinner::new();
    sync_spinner.set_visible(false);
    sync_spinner.set_valign(gtk::Align::Center);
    let sync_now_btn = gtk::Button::builder()
        .label("Sync Now")
        .css_classes(["pill"])
        .tooltip_text("Download recent activities and wellness into training load")
        .valign(gtk::Align::Center)
        .build();
    sync_action_row.add_suffix(&sync_spinner);
    sync_action_row.add_suffix(&sync_now_btn);
    garmin_group.add(&sync_action_row);

    let import_row = adw::ActionRow::builder()
        .title("Import History")
        .subtitle(IMPORT_SUBTITLE)
        .build();
    let import_spinner = gtk::Spinner::new();
    import_spinner.set_visible(false);
    import_spinner.set_valign(gtk::Align::Center);
    let import_btn = gtk::Button::builder()
        .label("Import")
        .css_classes(["pill"])
        .tooltip_text("Download your whole Garmin Connect history")
        .valign(gtk::Align::Center)
        .build();
    import_row.add_suffix(&import_spinner);
    import_row.add_suffix(&import_btn);
    garmin_group.add(&import_row);
    page.add(&garmin_group);

    // ── Intervals.icu ─────────────────────────────────────────────────────
    let icu_group = adw::PreferencesGroup::builder()
        .title("Intervals.icu")
        .description(
            "Your workout library, offered to the AI Coach, and an optional upload of each \
             ride. The API key is stored locally and sent only to intervals.icu. Find your \
             key at intervals.icu → Settings → API.",
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
    connect_expiry(&expiry_row, win, &pool, &rt_handle);
    // The main window's content is its toast overlay (window/mod.rs).
    let main_toasts = parent.content().and_downcast::<adw::ToastOverlay>();
    connect_import(
        &import_row,
        &import_btn,
        &import_spinner,
        win,
        main_toasts,
        &pool,
        &rt_handle,
    );

    page
}

/// Show the stored key expiry, and save what the rider applies.
fn connect_expiry(
    row: &adw::EntryRow,
    win: &adw::PreferencesWindow,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    let row_weak = row.downgrade();
    let pool_read = pool.clone();
    crate::ui::spawn_to_main(
        rt_handle,
        async move { settings::garmin_key_expires(&pool_read).await },
        move |stored| {
            let Some(row) = row_weak.upgrade() else {
                return;
            };
            match stored {
                Ok(Some(day)) => row.set_text(&day.format("%Y-%m-%d").to_string()),
                Ok(None) => {}
                Err(e) => tracing::error!("Could not read the Garmin key expiry: {e}"),
            }
        },
    );

    let pool = pool.clone();
    let rt_handle = rt_handle.clone();
    // Weak: the row lives inside the preferences window (CLAUDE.md §2.4).
    row.connect_apply(glib::clone!(
        #[weak]
        win,
        move |row| match parse_expiry(&row.text()) {
            Ok(expires) => spawn_write(
                &rt_handle,
                &pool,
                "the Garmin key expiry",
                move |pool| async move { settings::set_garmin_key_expires(&pool, expires).await },
            ),
            Err(()) => win.add_toast(
                adw::Toast::builder()
                    .title("Write the expiry date as year-month-day, e.g. 2027-01-31")
                    .timeout(5)
                    .build(),
            ),
        }
    ));
}

/// What an import reports to the GTK thread.
enum ImportMessage {
    Progress(garmin::Progress),
    Done(Result<garmin::ImportReport, String>),
}

/// One Preferences window's Import History row, held weakly: the window may
/// close while the import goes on.
#[derive(Clone)]
struct ImportWidgets {
    row: glib::WeakRef<adw::ActionRow>,
    spinner: glib::WeakRef<gtk::Spinner>,
    button: glib::WeakRef<gtk::Button>,
}

impl ImportWidgets {
    /// Show `status` (an import running) or the idle row (`None`). False once
    /// the window holding the row is gone.
    fn show(&self, status: Option<&str>) -> bool {
        let (Some(row), Some(spinner), Some(button)) = (
            self.row.upgrade(),
            self.spinner.upgrade(),
            self.button.upgrade(),
        ) else {
            return false;
        };
        row.set_subtitle(status.unwrap_or(IMPORT_SUBTITLE));
        spinner.set_visible(status.is_some());
        if status.is_some() {
            spinner.start();
        } else {
            spinner.stop();
        }
        button.set_sensitive(status.is_none());
        true
    }
}

/// The history import as the GTK thread sees it.
///
/// An import outlives the Preferences window it was started from — it carries
/// on when the window closes, and a window opened while it runs must show it —
/// so its state lives here rather than in any one window's widgets.
#[derive(Default)]
struct ImportView {
    /// What the row says while an import runs; `None` when none is.
    status: Option<String>,
    /// The Import History row of every Preferences window opened since.
    shown: Vec<ImportWidgets>,
}

thread_local! {
    static IMPORT_VIEW: std::cell::RefCell<ImportView> = std::cell::RefCell::default();
}

/// Record where the import has got to and show it in every open window.
fn set_import_status(status: Option<String>) {
    // Copied out so no borrow is held while the widgets update (CLAUDE.md §2.4).
    let shown = IMPORT_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        view.status = status.clone();
        view.shown.clone()
    });
    let alive: Vec<ImportWidgets> = shown
        .into_iter()
        .filter(|w| w.show(status.as_deref()))
        .collect();
    IMPORT_VIEW.with(|view| view.borrow_mut().shown = alive);
}

/// Add a newly built window's row, showing an import already running.
fn register_import_widgets(widgets: ImportWidgets) {
    let status = IMPORT_VIEW.with(|view| {
        let mut view = view.borrow_mut();
        view.shown.push(widgets.clone());
        view.status.clone()
    });
    widgets.show(status.as_deref());
}

/// Import the rider's whole Garmin history on demand.
///
/// The import runs on tokio and survives this window closing. Its progress is
/// shown under the row of every open Preferences window, and the summary goes
/// to the main window's toasts, which are there whatever the rider has open.
fn connect_import(
    row: &adw::ActionRow,
    button: &gtk::Button,
    spinner: &gtk::Spinner,
    win: &adw::PreferencesWindow,
    main_toasts: Option<adw::ToastOverlay>,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    register_import_widgets(ImportWidgets {
        row: row.downgrade(),
        spinner: spinner.downgrade(),
        button: button.downgrade(),
    });

    let pool = pool.clone();
    let rt_handle = rt_handle.clone();

    // Weak: this button lives inside the preferences window (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| {
            let Some(api_key) = garmin::api_key() else {
                win.add_toast(
                    adw::Toast::builder()
                        .title("Set your Garmin personal API key first")
                        .timeout(4)
                        .build(),
                );
                return;
            };
            // Taken here, not in the task, so a second click is refused at
            // once — from this window or any other.
            let Some(guard) = garmin::try_begin_import() else {
                win.add_toast(
                    adw::Toast::builder()
                        .title("A Garmin import is already running")
                        .timeout(4)
                        .build(),
                );
                return;
            };
            set_import_status(Some("Signing in to Garmin Connect…".to_string()));

            let (tx, rx) = async_channel::unbounded::<ImportMessage>();
            let pool_task = pool.clone();
            rt_handle.spawn(async move {
                let today = Local::now().date_naive();
                let progress_tx = tx.clone();
                let result =
                    garmin::import_history_live(&pool_task, &api_key, today, &guard, |p| {
                        // Unbounded, so a slow GTK thread never holds the import up.
                        let _ = progress_tx.try_send(ImportMessage::Progress(p));
                    })
                    .await
                    .map_err(|e| garmin_failure(&e));
                drop(guard);
                let _ = tx.send(ImportMessage::Done(result)).await;
            });

            // Owns nothing of this window, so the import reports to the end
            // however many windows open and close meanwhile.
            let main_toasts = main_toasts.clone();
            let win = win.downgrade();
            glib::MainContext::default().spawn_local(async move {
                let mut summary = None;
                while let Ok(message) = rx.recv().await {
                    match message {
                        ImportMessage::Progress(p) => set_import_status(Some(import_progress(p))),
                        ImportMessage::Done(result) => {
                            summary = Some(match result {
                                Ok(report) => import_summary(&report),
                                Err(e) => {
                                    tracing::error!("Garmin history import failed: {e}");
                                    format!("Import failed: {e}")
                                }
                            });
                            break;
                        }
                    }
                }
                set_import_status(None);
                let Some(title) = summary else { return };
                let toast = adw::Toast::builder().title(title).timeout(8).build();
                match (&main_toasts, win.upgrade()) {
                    (Some(overlay), _) => overlay.add_toast(toast),
                    (None, Some(win)) => win.add_toast(toast),
                    (None, None) => {}
                }
            });
        }
    ));
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

    // Weak: this button lives inside the preferences window (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |btn| {
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
        }
    ));
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

    // Weak: this button lives inside the preferences window (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |btn| {
            // Read the ID off the row rather than back out of the database: it is
            // already on screen, it saves a blocking read on the GTK thread, and an
            // ID just typed works without having to commit the row first.
            let athlete_id = id_row.text().trim().to_string();
            let garmin_key = garmin::api_key();
            let icu_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
                .unwrap_or(None)
                .unwrap_or_default();

            if garmin_key.is_none() && (athlete_id.is_empty() || icu_key.trim().is_empty()) {
                win.add_toast(
                    adw::Toast::builder()
                        .title(
                            "Set your Garmin key, or your Intervals.icu Athlete ID and key, first",
                        )
                        .timeout(4)
                        .build(),
                );
                return;
            }

            btn.set_sensitive(false);
            spinner.set_visible(true);
            spinner.start();

            let (tx, rx) = async_channel::bounded::<Result<SyncCounts, String>>(1);
            let pool_task = pool.clone();
            rt_handle.spawn(async move {
                match garmin_key {
                    Some(key) => sync_from_garmin(&pool_task, &key, &tx).await,
                    None => {
                        let _ = tx
                            .send(sync_activities(&pool_task, &athlete_id, &icu_key).await)
                            .await;
                    }
                }
            });

            let btn = btn.clone();
            let spinner = spinner.clone();
            let win = win.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(result) = rx.recv().await {
                    let toast = match result {
                        Ok(counts) => adw::Toast::builder()
                            .title(sync_summary(counts))
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
        }
    ));
}

/// How much one manual sync brought in.
#[derive(Debug, Clone, Copy)]
struct SyncCounts {
    /// The service the activities came from.
    source: &'static str,
    activities: usize,
    /// Activities Intervals.icu sent without their data, and so not stored.
    withheld: usize,
    /// Intervals.icu copies folded into their Garmin ones.
    merged: usize,
    wellness: usize,
}

/// Sync from Garmin Connect, send the counts, then fetch the new activities'
/// files — after the toast, since nothing on screen waits on them.
async fn sync_from_garmin(
    pool: &SqlitePool,
    api_key: &str,
    tx: &async_channel::Sender<Result<SyncCounts, String>>,
) {
    let source = match garmin::Live::connect(api_key).await {
        Ok(source) => source,
        Err(e) => {
            let _ = tx.send(Err(garmin_failure(&e))).await;
            return;
        }
    };
    let today = Local::now().date_naive();
    let report = garmin::sync(pool, &source, today, ACTIVITY_SYNC_DAYS, WELLNESS_SYNC_DAYS).await;
    let ok = report.is_ok();
    let _ = tx
        .send(
            report
                .map(|r| SyncCounts {
                    source: "Garmin Connect",
                    activities: r.activities,
                    withheld: 0,
                    merged: r.merged,
                    wellness: r.wellness,
                })
                .map_err(|e| garmin_failure(&e)),
        )
        .await;
    if ok {
        if let Err(e) = garmin::fetch_recent_files(pool, &source).await {
            tracing::error!("Garmin file pass failed: {e:#}");
        }
    }
}

/// Pull activities and wellness, returning how many of each were stored.
///
/// A wellness failure is not fatal: the activities are the point, and wellness
/// is only ever supporting detail.
async fn sync_activities(
    pool: &SqlitePool,
    athlete_id: &str,
    api_key: &str,
) -> Result<SyncCounts, String> {
    let newest = Local::now().date_naive();

    let activities = crate::ai::intervals::fetch_activities(
        athlete_id,
        api_key,
        newest - Duration::days(ACTIVITY_SYNC_DAYS),
        newest,
    )
    .await
    .map_err(|e| e.to_string())?;

    let stored = crate::ai::intervals::store_activities(pool, activities).await;

    let wellness_count = match crate::ai::intervals::fetch_wellness(
        athlete_id,
        api_key,
        newest - Duration::days(WELLNESS_SYNC_DAYS),
        newest,
    )
    .await
    {
        Ok(entries) => crate::ai::intervals::store_wellness(pool, entries).await,
        Err(e) => {
            tracing::warn!("Wellness sync failed (non-fatal): {e}");
            0
        }
    };

    Ok(SyncCounts {
        source: "Intervals.icu",
        activities: stored.saved,
        withheld: stored.withheld.len(),
        merged: 0,
        wellness: wellness_count,
    })
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
        let summary = sync_summary(SyncCounts {
            source: "Intervals.icu",
            activities: 12,
            withheld: 0,
            merged: 0,
            wellness: 30,
        });
        assert!(summary.contains("12 activities"), "got {summary}");
        assert!(summary.contains("30 wellness"), "got {summary}");
    }

    #[test]
    fn should_not_mention_wellness_when_none_came_back() {
        // Wellness is optional and its failure is non-fatal, so a zero means
        // "not available" rather than "you had none" — better left unsaid.
        let summary = sync_summary(SyncCounts {
            source: "Intervals.icu",
            activities: 12,
            withheld: 0,
            merged: 0,
            wellness: 0,
        });
        assert!(!summary.contains("wellness"), "got {summary}");
        assert!(summary.contains("12 activities"));
    }

    #[test]
    fn should_name_garmin_and_the_copies_it_folded_in() {
        let summary = sync_summary(SyncCounts {
            source: "Garmin Connect",
            activities: 9,
            withheld: 0,
            merged: 3,
            wellness: 0,
        });
        assert_eq!(
            summary,
            "Synced 9 activities from Garmin Connect — 3 Intervals.icu copies folded in"
        );
    }

    #[test]
    fn should_show_import_progress_without_dividing_by_nothing() {
        use garmin::Progress::*;
        assert_eq!(
            import_progress(Wellness {
                done: 0,
                total: 113
            }),
            "Wellness — 0 %"
        );
        assert_eq!(
            import_progress(Wellness {
                done: 112,
                total: 113
            }),
            "Wellness — 99 %"
        );
        assert_eq!(
            import_progress(Wellness { done: 0, total: 0 }),
            "Wellness — 100 %"
        );
        assert_eq!(
            import_progress(Files {
                done: 7,
                total: 1646
            }),
            "Activity files — 7 of 1646"
        );
        // A count past the total is clamped, never shown as 150 %.
        assert_eq!(percent(3, 2), 100);
    }

    #[test]
    fn should_say_when_an_import_needs_running_again() {
        let mut r = garmin::ImportReport {
            activities: 1646,
            wellness: 3165,
            ..Default::default()
        };
        r.files.fetched = 1645;
        assert_eq!(
            import_summary(&r),
            "Imported 1646 activities, 1645 activity files and 3165 days of wellness"
        );
        r.files.stopped = true;
        assert!(import_summary(&r).ends_with("pick up where it left off"));
        r.files.unauthorized = true;
        assert!(import_summary(&r).contains("refused the key"));
    }

    #[test]
    fn should_read_an_expiry_date_or_an_empty_row_and_nothing_else() {
        let day = chrono::NaiveDate::from_ymd_opt(2027, 1, 31).expect("hardcoded valid date");
        assert_eq!(parse_expiry(" 2027-01-31 "), Ok(Some(day)));
        assert_eq!(parse_expiry(""), Ok(None));
        assert_eq!(parse_expiry("   "), Ok(None));
        for bad in ["31/01/2027", "2027-02-30", "2027-1", "tomorrow"] {
            assert_eq!(parse_expiry(bad), Err(()), "{bad:?}");
        }
    }
}
