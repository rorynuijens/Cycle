//! The Data page: taking the training history out of Cycle, and putting one back.
//!
//! Both directions go through a file chooser rather than a path this code picks,
//! because under flatpak the app has no filesystem access of its own — the
//! chooser's portal is what lets an export land somewhere a rider will actually
//! find it, and what lets one be read back from wherever they kept it.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::path::PathBuf;

use crate::data::{paths, transfer};

pub fn build(
    win: &adw::PreferencesWindow,
    parent: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Data")
        .icon_name("drive-harddisk-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Your Training History")
        .description(
            "Every ride, workout, measurement and plan lives in a single file. \
             Exporting it is the only copy that exists outside this computer.",
        )
        .build();

    group.add(&export_row(win, pool.clone(), rt_handle.clone()));
    group.add(&import_row(win, parent, pool, rt_handle));
    page.add(&group);

    page.add(&location_group());
    page
}

// ── export ───────────────────────────────────────────────────────────────────

fn export_row(
    win: &adw::PreferencesWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) -> adw::ActionRow {
    let button = gtk::Button::builder()
        .label("Export…")
        .valign(gtk::Align::Center)
        .tooltip_text("Save your whole training history to a file")
        .build();

    let row = adw::ActionRow::builder()
        .title("Export History")
        .subtitle("Save everything to a file you choose")
        .activatable_widget(&button)
        .build();
    row.add_suffix(&button);

    // Weak: this button lives inside the preferences window (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Export Training History")
                .accept_label("Export")
                .initial_name(transfer::suggested_export_name(chrono::Local::now()))
                .build();
            dialog.set_filters(Some(&db_filters()));

            let win_parent = win.clone();
            let win_cb = win.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            dialog.save(
                Some(&win_parent),
                gtk::gio::Cancellable::NONE,
                move |result| {
                    // A cancelled chooser is not a failure worth reporting.
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else {
                        toast(&win_cb, "That location cannot be written to.");
                        return;
                    };
                    // The chooser removes a file the rider agreed to replace, and
                    // VACUUM INTO will not write over one — so a leftover here is
                    // stale and ours to clear.
                    let _ = std::fs::remove_file(&path);

                    let win_done = win_cb.clone();
                    let shown = path.clone();
                    crate::ui::spawn_to_main(
                        &rt_handle,
                        async move { transfer::export(&pool, &path).await },
                        move |result| match result {
                            Ok(()) => {
                                let name = shown
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                toast(&win_done, &format!("Exported to {name}"));
                            }
                            Err(e) => {
                                tracing::error!("History export failed: {e:#}");
                                toast(&win_done, "Your history could not be exported.");
                            }
                        },
                    );
                },
            );
        }
    ));

    row
}

// ── import ───────────────────────────────────────────────────────────────────

fn import_row(
    win: &adw::PreferencesWindow,
    parent: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
) -> adw::ActionRow {
    let button = gtk::Button::builder()
        .label("Import…")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .tooltip_text("Replace your training history with an exported file")
        .build();

    let row = adw::ActionRow::builder()
        .title("Import History")
        .subtitle("Replace everything in Cycle with an exported file")
        .activatable_widget(&button)
        .build();
    row.add_suffix(&button);

    let parent = parent.clone();
    // Weak: this button lives inside the preferences window (CLAUDE.md §2.4).
    button.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Import Training History")
                .accept_label("Open")
                .build();
            dialog.set_filters(Some(&db_filters()));

            let win_parent = win.clone();
            let win_cb = win.clone();
            let parent = parent.clone();
            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            dialog.open(
                Some(&win_parent),
                gtk::gio::Cancellable::NONE,
                move |result| {
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else {
                        toast(&win_cb, "That file cannot be read.");
                        return;
                    };

                    // Nothing is touched until the file has been read and the rider
                    // has seen what is in it.
                    let win_done = win_cb.clone();
                    let parent_done = parent.clone();
                    let pool_done = pool.clone();
                    let rt_done = rt_handle.clone();
                    let candidate = path.clone();
                    crate::ui::spawn_to_main(
                        &rt_handle,
                        async move { transfer::inspect(&path).await },
                        move |result| match result {
                            Ok(summary) => confirm_import(
                                &win_done,
                                &parent_done,
                                pool_done,
                                rt_done,
                                candidate,
                                summary,
                            ),
                            Err(e) => {
                                tracing::warn!("Rejected import candidate: {e:#}");
                                let dialog = adw::AlertDialog::builder()
                                    .heading("This file cannot be imported")
                                    .body(format!("{e}"))
                                    .build();
                                dialog.add_response("ok", "_OK");
                                dialog.set_default_response(Some("ok"));
                                dialog.present(Some(&win_done));
                            }
                        },
                    );
                },
            );
        }
    ));

    row
}

/// Show what the file holds and what replacing costs, before doing it.
fn confirm_import(
    win: &adw::PreferencesWindow,
    parent: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    candidate: PathBuf,
    summary: transfer::ImportSummary,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Replace your training history?")
        .body(format!(
            "The file holds {}, {} tracked days and {} workouts.\n\n\
             Everything currently in Cycle is replaced. A copy of what you have \
             now is saved beside the database first, and Cycle closes so the \
             change takes effect.",
            summary.ride_span(),
            summary.wellness_days,
            summary.workouts,
        ))
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("import", "_Replace");
    dialog.set_response_appearance("import", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let win_present = win.clone();
    let win = win.clone();
    let parent = parent.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response != "import" {
            return;
        }
        dialog.set_can_close(false);

        let win_done = win.clone();
        let parent_done = parent.clone();
        let pool = pool.clone();
        let candidate = candidate.clone();
        crate::ui::spawn_to_main(
            &rt_handle,
            async move { transfer::replace_with(pool, &candidate).await },
            move |result| match result {
                Ok(replaced) => {
                    let kept = replaced
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    // From here the pool is closed and the file it was reading
                    // has been swapped, so every database call in this process
                    // will fail. Nothing may depend on the rider dismissing a
                    // dialog: the window that could start another import is
                    // closed now, and quitting is scheduled whatever they do
                    // with the message — including ignoring it entirely.
                    win_done.close();

                    let done = adw::AlertDialog::builder()
                        .heading("History imported")
                        .body(format!(
                            "Cycle closes now so it reopens on the imported \
                             history.\n\nWhat you had before is kept as {kept}."
                        ))
                        .build();
                    done.add_response("quit", "_Close Cycle");
                    done.set_default_response(Some("quit"));
                    done.set_close_response("quit");

                    let quit = {
                        let parent_quit = parent_done.clone();
                        move || {
                            if let Some(app) = parent_quit.application() {
                                app.quit();
                            } else {
                                // No application to ask: close the window instead,
                                // rather than leave a dead one on screen.
                                parent_quit.close();
                            }
                        }
                    };
                    let quit_on_response = quit.clone();
                    done.connect_response(None, move |_, _| quit_on_response());
                    done.present(Some(&parent_done));

                    // Backstop: if the message is never answered, leave anyway.
                    gtk::glib::timeout_add_seconds_local_once(20, quit);
                }
                Err(e) => {
                    tracing::error!("History import failed: {e:#}");
                    let failed = adw::AlertDialog::builder()
                        .heading("Import failed")
                        .body(format!(
                            "{e}\n\nYour history has not been replaced.\n\n\
                             If Cycle behaves oddly from here, close it and reopen it."
                        ))
                        .build();
                    failed.add_response("ok", "_OK");
                    failed.set_default_response(Some("ok"));
                    failed.present(Some(&win_done));
                }
            },
        );
    });

    dialog.present(Some(&win_present));
}

// ── where the data lives ─────────────────────────────────────────────────────

/// Shows the folder holding the database, with a way to open it.
///
/// Under flatpak this path is inside `~/.var/app`, which is not somewhere a
/// rider would think to look for something they care about keeping.
fn location_group() -> adw::PreferencesGroup {
    let dir = paths::data_dir();
    let group = adw::PreferencesGroup::builder().title("Location").build();

    let button = gtk::Button::builder()
        .icon_name("folder-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .tooltip_text("Show this folder in Files")
        .build();

    let row = adw::ActionRow::builder()
        .title("Database Folder")
        .subtitle(dir.to_string_lossy())
        .subtitle_selectable(true)
        .build();
    row.add_suffix(&button);

    button.connect_clicked(move |btn| {
        let file = gtk::gio::File::for_path(&dir);
        let parent = btn.root().and_downcast::<gtk::Window>();
        gtk::FileLauncher::new(Some(&file)).launch(
            parent.as_ref(),
            gtk::gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    tracing::warn!("Could not open the data folder: {e}");
                }
            },
        );
    });

    group.add(&row);
    group
}

// ── shared bits ──────────────────────────────────────────────────────────────

fn db_filters() -> gtk::gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Cycle history"));
    filter.add_pattern("*.db");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters
}

fn toast(win: &adw::PreferencesWindow, message: &str) {
    win.add_toast(adw::Toast::builder().title(message).build());
}
