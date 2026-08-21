//! Applying and undoing the program's easing, from the calendar.
//!
//! Both the week list and the entry detail dialog offer these, and both go
//! through here so the wording, the logging and the reload behave identically
//! wherever the rider presses the button.
//!
//! Neither of these decides anything: the swap was chosen by
//! [`crate::training::program::suggest`], and these only carry it to the
//! database.

use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::db;

/// Swap a planned session for the easier one the program suggested.
pub fn apply_easing(
    pool: SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    entry_id: i64,
    to_workout_id: i64,
    to_name: String,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    reload: Rc<dyn Fn()>,
) {
    crate::ui::spawn_to_main(
        rt_handle,
        async move { db::apply_adjustment(&pool, entry_id, to_workout_id).await },
        move |result| {
            match result {
                Ok(true) => {
                    on_toast(
                        adw::Toast::builder()
                            .title(format!("Eased to {to_name}"))
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                // Ridden or deleted since the suggestion was drawn. Reload
                // anyway — that is what clears the row now offering it.
                Ok(false) => {
                    tracing::warn!("easing {entry_id} changed nothing — already ridden or gone");
                    on_toast(
                        adw::Toast::builder()
                            .title("That session can no longer be changed")
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                Err(e) => {
                    tracing::error!("applying adjustment to {entry_id}: {e}");
                    on_toast(
                        adw::Toast::builder()
                            .title("Could not ease that session")
                            .timeout(5)
                            .build(),
                    );
                }
            };
        },
    );
}

/// Put a session back to what the program originally asked for.
pub fn undo_easing(
    pool: SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    entry_id: i64,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    reload: Rc<dyn Fn()>,
) {
    crate::ui::spawn_to_main(
        rt_handle,
        async move { db::revert_adjustment(&pool, entry_id).await },
        move |result| {
            match result {
                Ok(true) => {
                    on_toast(
                        adw::Toast::builder()
                            .title("Session put back as planned")
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                Ok(false) => {
                    tracing::warn!("undo on {entry_id} changed nothing — already ridden or gone");
                    on_toast(
                        adw::Toast::builder()
                            .title("That session can no longer be changed")
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                Err(e) => {
                    tracing::error!("reverting adjustment {entry_id}: {e}");
                    on_toast(
                        adw::Toast::builder()
                            .title("Could not put that session back")
                            .timeout(5)
                            .build(),
                    );
                }
            };
        },
    );
}
