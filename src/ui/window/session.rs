//! Keeping a ride safe: checkpointing it as it happens, and offering it back
//! when the app never got to finish it.

use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::data::{athlete::AthleteProfile, db, workout::Workout};
use crate::training::engine::WorkoutEngine;
use crate::ui::pages::route_player::RoutePlayerPage;
use crate::ui::pages::summary::SummaryPage;

/// How often a ride in progress is written to disk. The trade is how much of a
/// ride a crash can cost against how often a multi-hundred-KB blob is rewritten;
/// 30 s keeps the worst case to half a minute of pedalling.
const CHECKPOINT_INTERVAL_SECS: u64 = 30;

/// Start writing the ride in progress to its own row every 30 s.
#[allow(clippy::too_many_arguments)]
pub fn start_checkpoint_timer(
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    engine_rc: Rc<RefCell<WorkoutEngine>>,
    route_player_rc: Rc<RoutePlayerPage>,
    workout_active: Rc<Cell<bool>>,
    route_timer_alive: Rc<Cell<bool>>,
    checkpoint_id: Rc<Cell<Option<i64>>>,
) {
    let checkpoint_engine = Rc::clone(&engine_rc);
    let checkpoint_route = Rc::clone(&route_player_rc);
    let checkpoint_workout_active = Rc::clone(&workout_active);
    let checkpoint_route_active = Rc::clone(&route_timer_alive);
    let checkpoint_pool = pool.clone();
    let checkpoint_rt = rt_handle.clone();
    let checkpoint_id_timer = Rc::clone(&checkpoint_id);
    glib::timeout_add_local(Duration::from_secs(CHECKPOINT_INTERVAL_SECS), move || {
        let snapshot = if checkpoint_workout_active.get() {
            let engine = checkpoint_engine.borrow();
            let session = engine.session.clone();
            drop(engine);
            Some(session)
        } else if checkpoint_route_active.get() {
            checkpoint_route.live_session_snapshot()
        } else {
            None
        };

        // Nothing worth writing until the rider has actually produced data —
        // this keeps an opened-but-unridden workout out of the recovery list.
        let Some(session) = snapshot.filter(|s| !s.data_points.is_empty()) else {
            return glib::ControlFlow::Continue;
        };

        let existing = checkpoint_id_timer.get();
        let pool = checkpoint_pool.clone();
        let cleanup_pool = checkpoint_pool.clone();
        let cleanup_rt = checkpoint_rt.clone();
        let id_cell = Rc::clone(&checkpoint_id_timer);
        let wa = Rc::clone(&checkpoint_workout_active);
        let ra = Rc::clone(&checkpoint_route_active);
        crate::ui::spawn_to_main(
            &checkpoint_rt,
            async move { db::checkpoint_session(&pool, existing, &session).await },
            move |result| match result {
                Ok(row_id) => {
                    if wa.get() || ra.get() {
                        id_cell.set(Some(row_id));
                    } else if existing.is_none() {
                        // The ride finished while this first checkpoint was in
                        // flight, so it wrote its own row and this one is a
                        // duplicate that would surface as a phantom recovery.
                        cleanup_rt.spawn(async move {
                            if let Err(e) = db::delete_session(&cleanup_pool, row_id).await {
                                tracing::error!("stale checkpoint cleanup failed: {e}");
                            }
                        });
                    }
                }
                Err(e) => tracing::error!("checkpoint failed: {e}"),
            },
        );
        glib::ControlFlow::Continue
    });
}

/// Ask what to do with a ride the app never finished writing.
///
/// Continuing puts the rider back on the workout screen at the second the
/// ride stopped, behind the usual ten-second power gate. Keeping it instead
/// files the ride as it stands, stamping an end time derived from the last
/// recorded second so its duration reflects what was actually ridden rather
/// than the gap until the app was reopened.
///
/// `workout` is the plan the ride was following, and is `None` for a route
/// ride or one whose workout has since been deleted — continuing needs the
/// plan, so it is only offered when there is one.
#[allow(clippy::too_many_arguments)]
pub fn offer_recovery(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    rt: tokio::runtime::Handle,
    record: db::SessionRecord,
    workout: Option<Workout>,
    reload: Rc<dyn Fn()>,
    on_resume: Rc<dyn Fn(Workout, crate::data::session::Session, i64)>,
) {
    let session = record.session;
    let ridden_secs = session
        .data_points
        .last()
        .map(|p| p.elapsed_secs)
        .unwrap_or(0);
    // A checkpoint is only written once there is data, so an empty row means
    // something went wrong rather than a ride worth offering back.
    if ridden_secs == 0 {
        let pool_empty = pool.clone();
        rt.spawn(async move {
            let _ = db::delete_session(&pool_empty, session.id).await;
        });
        return;
    }

    let when = session
        .started_at
        .with_timezone(&chrono::Local)
        .format("%A %-d %B, %H:%M");
    let name = record.workout_name.as_deref().unwrap_or("Route ride");
    let ridden = crate::training::engine::WorkoutEngine::format_duration(ridden_secs);
    let body = match &workout {
        Some(w) => {
            let remaining = w.duration_secs.saturating_sub(ridden_secs);
            format!(
                "“{name}” from {when} was still recording when Cycle closed.\n\n\
                 {ridden} was saved, with {} still to ride. Continuing puts you \
                 back on the workout with ten seconds to get going.",
                crate::training::engine::WorkoutEngine::format_duration(remaining)
            )
        }
        None => format!(
            "“{name}” from {when} was still recording when Cycle closed.\n\n\
             {ridden} of riding was saved."
        ),
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Recover the interrupted ride?")
        .body(body)
        .build();
    dialog.add_response("discard", "_Discard");
    dialog.add_response("keep", "_Keep Ride");
    dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    // Continuing is only possible with the plan in hand, and there is nothing
    // left to ride once the workout has run its full length.
    let can_continue = workout
        .as_ref()
        .is_some_and(|w| w.duration_secs > ridden_secs);
    if can_continue {
        dialog.add_response("continue", "_Continue Ride");
        dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("continue"));
    } else {
        dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("keep"));
    }
    // Dismissing without choosing leaves the row untouched, so the offer
    // comes back next launch rather than silently resolving either way.
    dialog.set_close_response("later");

    let session_id = session.id;
    let ended_at =
        (session.started_at + chrono::Duration::seconds(ridden_secs as i64)).to_rfc3339();
    let resume_session = session.clone();
    dialog.connect_response(None, move |_, response| {
        let pool = pool.clone();
        let reload = Rc::clone(&reload);
        match response {
            "continue" => {
                // The row stays unfinished and is handed to the ride, which
                // keeps checkpointing to it and finalises it when it ends.
                if let Some(w) = workout.clone() {
                    on_resume(w, resume_session.clone(), session_id);
                }
            }
            "keep" => {
                let ended_at = ended_at.clone();
                crate::ui::spawn_to_main(
                    &rt,
                    async move { db::finalise_session(&pool, session_id, &ended_at).await },
                    move |r| match r {
                        Ok(()) => reload(),
                        Err(e) => tracing::error!("recovering ride failed: {e}"),
                    },
                );
            }
            "discard" => {
                rt.spawn(async move {
                    if let Err(e) = db::delete_session(&pool, session_id).await {
                        tracing::error!("discarding interrupted ride failed: {e}");
                    }
                });
            }
            _ => {}
        }
    });
    dialog.present(Some(window));
}

/// Build the closure that ends a ride.
///
/// Summary page, RPE prompt, save and upload — the same tail whether the rider
/// was following a plan or a route, so both paths funnel through here.
/// `segments` is `None` for a route ride, which has no plan to compare against.
#[allow(clippy::too_many_arguments)]
pub fn finish_session_closure(
    pool_for_complete: SqlitePool,
    rt_for_complete: tokio::runtime::Handle,
    window_for_rpe: adw::ApplicationWindow,
    toast_overlay_for_complete: adw::ToastOverlay,
    stack_for_complete: adw::ViewStack,
    summary_for_complete: SummaryPage,
    athlete_for_complete: Rc<RefCell<AthleteProfile>>,
    checkpoint_id_complete: Rc<Cell<Option<i64>>>,
) -> super::FinishSession {
    Rc::new(move |session, name: String, segments| {
        // Borrowed, not captured: the profile must be whatever it is now, so
        // the summary and the FIT export it feeds carry the rider's current
        // weight and HR range rather than the values held at startup. The
        // borrow is dropped before the summary page can emit any signal.
        let athlete_now = athlete_for_complete.borrow().clone();
        // The FTP the ride was actually executed at — the stamped value wins
        // over the profile, so a bump taken mid-ride is not re-suggested.
        let ftp = session.ftp_watts.unwrap_or(athlete_now.ftp_watts);
        summary_for_complete.update(&session, &name, &athlete_now, segments.as_deref());
        stack_for_complete.set_visible_child_name("summary");

        // FTP auto-suggestion based on 20-minute best power
        if let Some(peak_20) = session.peak_power_for_duration(1200) {
            let suggested = (peak_20 as f32 * 0.95) as u32;
            if suggested > ftp + 5 {
                toast_overlay_for_complete.add_toast(
                    adw::Toast::builder()
                        .title(format!(
                            "Your 20-min best suggests an FTP of {} W (currently {} W). \
                         Update in Preferences.",
                            suggested, ftp
                        ))
                        .timeout(10)
                        .build(),
                );
            }
        }

        let pool = pool_for_complete.clone();
        let workout_id = session.workout_id;

        // session_id is set by the tokio save task and read by the RPE callback.
        // The session save takes ~10 ms; the RPE dialog requires human interaction
        // (several seconds minimum), so the Arc will be populated well before the
        // user can submit — the race window is negligible.
        let session_id_arc: std::sync::Arc<std::sync::Mutex<Option<i64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let session_id_for_rpe = std::sync::Arc::clone(&session_id_arc);
        let session_id_for_task = std::sync::Arc::clone(&session_id_arc);

        // Show the RPE questionnaire immediately after the workout ends.
        let pool_rpe = pool_for_complete.clone();
        let rt_rpe = rt_for_complete.clone();
        let summary_for_rpe = summary_for_complete.clone();
        crate::ui::widgets::rpe_dialog::show(&window_for_rpe, move |rpe| {
            summary_for_rpe.show_rpe_icon(rpe);

            if let Some(sid) = *session_id_for_rpe
                .lock()
                .expect("session_id_arc cannot be poisoned")
            {
                let p = pool_rpe.clone();
                rt_rpe.spawn(async move {
                    if let Err(e) = db::save_session_rpe(&p, sid, rpe).await {
                        tracing::error!("save_session_rpe failed: {e}");
                    }
                });
            } else {
                tracing::warn!("RPE submitted before session was saved — RPE not persisted");
            }
        });

        // Take the checkpoint row so this ride finalises the row it has been
        // writing to all along. Cleared immediately: the ride is over, and a
        // later checkpoint must never reuse this id.
        let existing_row = checkpoint_id_complete.take();

        rt_for_complete.spawn(async move {
            match db::upsert_session(&pool, existing_row, &session).await {
                Err(e) => {
                    tracing::error!("save_session failed: {e}");
                }
                Ok(session_id) => {
                    *session_id_for_task
                        .lock()
                        .expect("session_id_arc cannot be poisoned") = Some(session_id);
                    tracing::info!("Session saved");
                    if let Some(wid) = workout_id {
                        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                        if let Err(e) = db::complete_today_calendar_entry(&pool, wid, &today).await
                        {
                            tracing::error!("complete_today_calendar_entry failed: {e}");
                        }
                    }
                    // Intervals.icu upload — send the full FIT file so Intervals.icu gets
                    // time-series data (power curve, HR, cadence) and can sync it to
                    // Garmin Connect with complete activity data.
                    let upload_enabled = db::get_setting(&pool, "intervals.upload")
                        .await
                        .unwrap_or(None)
                        .map(|v| v == "1")
                        .unwrap_or(false);
                    if upload_enabled {
                        let api_key = crate::data::keystore::get_secret(
                            crate::data::keystore::KEY_INTERVALS_API,
                        )
                        .unwrap_or(None)
                        .unwrap_or_default();
                        let athlete_id = db::get_setting(&pool, "intervals.athlete_id")
                            .await
                            .unwrap_or(None)
                            .unwrap_or_default();
                        if !api_key.trim().is_empty() && !athlete_id.trim().is_empty() {
                            // Loaded fresh rather than captured at startup so the
                            // export carries the FTP and heart-rate limits in
                            // force now, which is what training load is scaled to.
                            //
                            // A failed read cancels the upload. Falling back to a
                            // default profile would send a file whose training-load
                            // figure is scaled to FTP 200, and Intervals.icu and
                            // Garmin both read that figure out of the file rather
                            // than recomputing it — so a wrong number would stick.
                            // The ride is already saved locally and can be uploaded
                            // again by hand.
                            let profile = match db::load_or_create_athlete(&pool).await {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::error!(
                                        "Could not read the athlete profile; \
                                     skipping the Intervals.icu upload: {e}"
                                    );
                                    return;
                                }
                            };
                            let fit_bytes = crate::data::fit::encode_session(&session, &profile);
                            match crate::ai::intervals::upload_fit_activity(
                                &athlete_id,
                                &api_key,
                                fit_bytes,
                                &name,
                            )
                            .await
                            {
                                Ok(()) => {
                                    tracing::info!("Session uploaded to Intervals.icu");
                                    // Mark the session so compute_load_metrics skips its
                                    // local TSS — the same workout is now in
                                    // intervals_activities and would otherwise be counted twice.
                                    if let Err(e) =
                                        db::mark_session_uploaded_to_icu(&pool, session_id).await
                                    {
                                        tracing::error!("mark_session_uploaded_to_icu: {e}");
                                    }
                                }
                                Err(e) => tracing::error!("Intervals.icu upload failed: {e}"),
                            }
                        }
                    }
                } // Ok(session_id)
            } // match save_session
        });
    })
}
