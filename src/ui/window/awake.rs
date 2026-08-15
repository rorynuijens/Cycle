//! Keeping the screen lit for as long as the ride lasts.
//!
//! A rider on a trainer touches nothing for an hour: no keyboard, no mouse, no
//! pointer moving anywhere. To the session that is indistinguishable from a
//! machine nobody is sitting at, so the screen dims and blanks partway through
//! an interval — exactly when the target watts matter most. Every other trainer
//! app holds the session awake for the duration of a ride, and so does this one.
//!
//! Inhibition goes through `gtk_application_inhibit`, which routes to
//! `org.freedesktop.portal.Inhibit` inside a Flatpak sandbox and to the session
//! manager outside one. That means no new dependency and no new `finish-args`
//! line — but it can still be refused, so a failure is logged and the ride
//! carries on rather than being treated as fatal.

use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How often the ride flags are re-read.
///
/// A poll rather than a callback: the two flags are plain `Cell`s written from
/// several places across the window and the route player, and threading a
/// change hook through all of them would add an argument to signatures that are
/// already long. A second of lag either way is invisible against a blank
/// timeout measured in minutes, and the check is two `Cell` reads.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Holds an idle inhibitor, acquiring and releasing it at most once per ride.
struct KeepAwake {
    app: adw::Application,
    window: adw::ApplicationWindow,
    /// The session's handle on our inhibitor. `None` when nothing is held.
    cookie: Cell<Option<u32>>,
}

impl KeepAwake {
    fn acquire(&self) {
        if self.cookie.get().is_some() {
            return;
        }
        let cookie = self.app.inhibit(
            Some(&self.window),
            gtk::ApplicationInhibitFlags::IDLE,
            Some("Ride in progress"),
        );
        // Zero means the session refused — most likely a portal that is not
        // running. Nothing here can fix that, and a ride is not worth
        // interrupting over it, so note it once and carry on with a dimming
        // screen.
        if cookie == 0 {
            tracing::warn!("Could not inhibit screen blanking for this ride");
            return;
        }
        tracing::debug!("Screen blanking inhibited for the ride");
        self.cookie.set(Some(cookie));
    }

    fn release(&self) {
        if let Some(cookie) = self.cookie.take() {
            self.app.uninhibit(cookie);
            tracing::debug!("Screen blanking inhibit released");
        }
    }
}

/// Hold the screen awake for as long as either ride flag is set.
///
/// Both a structured workout and a route ride count, and a paused ride still
/// counts: a rider standing over the bike between efforts has not stopped, and
/// coming back to a blanked screen would be the same failure a beat later.
pub fn keep_awake_during_rides(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    workout_active: Rc<Cell<bool>>,
    route_timer_alive: Rc<Cell<bool>>,
) {
    let guard = Rc::new(KeepAwake {
        app: app.clone(),
        window: window.clone(),
        cookie: Cell::new(None),
    });

    // The timeout outlives nothing it owns — the application and the window both
    // outlive the ride loop — so a strong capture here creates no cycle the way
    // a handler on a child widget would (CLAUDE.md §2.4).
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if workout_active.get() || route_timer_alive.get() {
            guard.acquire();
        } else {
            guard.release();
        }
        glib::ControlFlow::Continue
    });
}
