//! Settling a planned session from the calendar: easing it, undoing that, and
//! marking it done by hand.
//!
//! The week list, the entry detail dialog and the coaching program card all
//! offer these, and all go through here so the wording, the logging and the
//! reload behave identically wherever the rider presses the button.
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

/// Mark a planned session done, or put it back to not done.
///
/// Settles the session against the *plan* only. It banks no training load —
/// fitness is computed from recorded rides, never from calendar entries — so
/// this closes the day without pretending a ride happened. Real rides start
/// closing their own days in 0.7.0.
///
/// `on_settled` is called with the new state once the write lands, and only
/// then. The week list and the coaching card pass `None`: `reload` rebuilds
/// their rows from the database, so they redraw for free. The detail dialog
/// cannot — it stays on screen holding the entry it was opened with, and a row
/// that still says "Marked done" after being un-marked is the write looking
/// like it failed.
pub fn set_session_done(
    pool: SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    entry_id: i64,
    done: bool,
    on_toast: Rc<dyn Fn(adw::Toast)>,
    reload: Rc<dyn Fn()>,
    on_settled: Option<Rc<dyn Fn(bool)>>,
) {
    crate::ui::spawn_to_main(
        rt_handle,
        async move { db::set_entry_completed(&pool, entry_id, done).await },
        move |result| {
            match result {
                Ok(true) => {
                    if let Some(settled) = &on_settled {
                        settled(done);
                    }
                    on_toast(
                        adw::Toast::builder()
                            .title(if done {
                                "Marked done"
                            } else {
                                "Marked as not done"
                            })
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                // Deleted since the row was drawn. Reload anyway — that is what
                // clears the stale row still offering the button.
                Ok(false) => {
                    tracing::warn!("marking {entry_id} changed nothing — the entry is gone");
                    on_toast(
                        adw::Toast::builder()
                            .title("That session is no longer on your calendar")
                            .timeout(5)
                            .build(),
                    );
                    reload();
                }
                Err(e) => {
                    tracing::error!("marking entry {entry_id} done={done}: {e}");
                    on_toast(
                        adw::Toast::builder()
                            .title("Could not update that session")
                            .timeout(5)
                            .build(),
                    );
                }
            };
        },
    );
}
