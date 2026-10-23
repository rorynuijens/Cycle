//! The one way an accepted FTP reaches the rider's profile.
//!
//! Both offers of a new FTP — the ramp test's result and the Fitness page's
//! check-in — end here, so the profile and the FTP history cannot drift apart.

use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::athlete::AthleteProfile;
use crate::data::db;

/// Make `new_ftp` the rider's FTP, and say so with a toast.
///
/// `source` is what [`db::log_ftp_change`] records ("ramp_test", "suggestion").
/// Runs on the GTK main thread; the database writes go to the runtime.
pub fn accept(
    pool: &SqlitePool,
    rt: &tokio::runtime::Handle,
    athlete: &Rc<RefCell<AthleteProfile>>,
    on_toast: &dyn Fn(adw::Toast),
    new_ftp: u32,
    source: &'static str,
    note: &'static str,
) {
    // One cell, read by every page and by the running engine, so writing it is
    // the whole UI side of the change.
    let profile = {
        let mut a = athlete.borrow_mut();
        a.ftp_watts = new_ftp;
        a.clone()
    };
    on_toast(
        adw::Toast::builder()
            .title(format!("FTP set to {new_ftp} W"))
            .timeout(5)
            .build(),
    );
    let pool = pool.clone();
    rt.spawn(async move {
        if let Err(e) = db::update_athlete(&pool, &profile).await {
            tracing::error!("update_athlete failed after accepting an FTP ({source}): {e}");
            return;
        }
        // Logged only once the profile write succeeded, so the history never
        // claims a change the profile did not take.
        if let Err(e) = db::log_ftp_change(&pool, new_ftp, source, note).await {
            tracing::error!("log_ftp_change failed after accepting an FTP ({source}): {e}");
        }
    });
}
