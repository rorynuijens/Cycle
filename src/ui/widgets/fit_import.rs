//! Shared "import a FIT file" flow.
//!
//! This is wired from both the History page and the Calendar page, so the
//! file-chooser → parse → de-duplicate → save → toast sequence lives here once
//! rather than being copy-pasted into each.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use sqlx::SqlitePool;

use crate::data::db;

/// Wire `button` to the FIT-import flow.
///
/// On a successful import the activity is saved and `on_imported` is invoked on
/// the GTK main thread (use it to refresh the surrounding view). All database
/// work runs on the tokio runtime via [`crate::ui::spawn_to_main`], so the GLib
/// loop is never blocked. User-facing outcomes are reported through `on_toast`.
pub fn connect_fit_import_button(
    button: &gtk::Button,
    pool: SqlitePool,
    rt: tokio::runtime::Handle,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    on_imported: Rc<dyn Fn()>,
) {
    button.connect_clicked(move |btn| {
        let fit_filter = gtk::FileFilter::new();
        fit_filter.set_name(Some("FIT files"));
        fit_filter.add_pattern("*.fit");
        fit_filter.add_pattern("*.FIT");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&fit_filter);

        let dialog = gtk::FileDialog::builder()
            .title("Import FIT File")
            .accept_label("Import")
            .build();
        dialog.set_filters(Some(&filters));

        let window = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());

        let pool = pool.clone();
        let rt = rt.clone();
        let on_toast = Rc::clone(&on_toast);
        let on_imported = Rc::clone(&on_imported);

        dialog.open(window.as_ref(), None::<&gio::Cancellable>, move |result| {
            let file = match result {
                Ok(f) => f,
                Err(_) => return, // user cancelled
            };
            let Some(path) = file.path() else {
                on_toast(
                    adw::Toast::builder()
                        .title("Could not read file path")
                        .timeout(6)
                        .build(),
                );
                return;
            };

            let session = match crate::data::fit::import_fit_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("FIT import parse error: {e}");
                    on_toast(
                        adw::Toast::builder()
                            .title(format!("Could not read FIT file: {e}"))
                            .timeout(8)
                            .build(),
                    );
                    return;
                }
            };

            // Duplicate check then save run together on the tokio runtime so the
            // GLib loop is never blocked; the UI is updated on the main thread once
            // the work completes.
            enum ImportOutcome {
                Duplicate,
                Saved,
                SaveFailed(String),
            }
            crate::ui::spawn_to_main(
                &rt,
                async move {
                    // On a duplicate-check error, log and still attempt the save
                    // (best-effort behaviour).
                    match db::session_exists_at(&pool, &session.started_at).await {
                        Ok(true) => return ImportOutcome::Duplicate,
                        Err(e) => tracing::error!("session_exists_at failed: {e}"),
                        Ok(false) => {}
                    }
                    match db::save_session(&pool, &session).await {
                        Ok(_) => ImportOutcome::Saved,
                        Err(e) => {
                            tracing::error!("save_session after FIT import failed: {e}");
                            ImportOutcome::SaveFailed(e.to_string())
                        }
                    }
                },
                move |outcome| match outcome {
                    ImportOutcome::Duplicate => on_toast(
                        adw::Toast::builder()
                            .title("This activity has already been imported")
                            .timeout(4)
                            .build(),
                    ),
                    ImportOutcome::Saved => {
                        on_toast(
                            adw::Toast::builder()
                                .title("Activity imported")
                                .timeout(4)
                                .build(),
                        );
                        on_imported();
                    }
                    ImportOutcome::SaveFailed(e) => on_toast(
                        adw::Toast::builder()
                            .title(format!("Failed to save activity: {e}"))
                            .timeout(8)
                            .build(),
                    ),
                },
            );
        });
    });
}
