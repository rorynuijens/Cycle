//! The one daily brief, and everyone waiting to hear about it.
//!
//! Three cards on three pages show slices of one request. They are built at
//! different moments during start-up, long before the request comes back, and
//! any of them can be rebuilt when the rider navigates. So none of them owns
//! the brief: this does, and they subscribe.
//!
//! Lives on the GTK main thread and is only ever touched from it, which is why
//! it is `Rc<RefCell<…>>` and not `Arc<Mutex<…>>` (CLAUDE.md §2.4). The work it
//! starts runs on the tokio runtime and comes back through [`spawn_to_main`].
//!
//! # What may spend the rider's money
//!
//! Exactly two things: [`BriefStore::start`], once per day at launch, and
//! [`BriefStore::refresh`], when the rider presses the button. Everything else —
//! notably [`BriefStore::revalidate`], which runs on every page navigation —
//! only ever reads the database and marks the brief out of date.

use std::cell::RefCell;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::ai::brief::{self, DailyBrief, StartupAction};
use crate::data::{ai_cache, athlete::AthleteProfile, keystore};
use crate::ui::{spawn_to_main, AiFailure};

/// Where the brief has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BriefStatus {
    /// Nothing has happened yet, or what is shown is current.
    #[default]
    Idle,
    /// A request is out.
    Loading,
    /// What is shown was written before something the coach was told changed.
    OutOfDate,
    /// There is no API key to ask with.
    NoApiKey,
    /// The last attempt did not produce a brief.
    Failed(AiFailure),
}

/// What every card renders from.
#[derive(Debug, Clone, Default)]
pub struct BriefState {
    /// `None` until one has been written, or when none could be.
    pub brief: Option<DailyBrief>,
    pub status: BriefStatus,
}

impl BriefState {
    /// The line under a card's title saying where its text came from.
    ///
    /// One sentence, written once, so three cards cannot describe the same
    /// state three different ways.
    pub fn provenance(&self) -> &'static str {
        match self.status {
            BriefStatus::Loading => "Asking your coach…",
            BriefStatus::OutOfDate => "Out of date — your training has changed since",
            BriefStatus::NoApiKey => "No AI provider key configured",
            BriefStatus::Failed(_) => "Could not reach your coach",
            BriefStatus::Idle if self.brief.is_some() => "From this morning's brief",
            BriefStatus::Idle => "No brief yet",
        }
    }

    /// Whether a card should offer its Refresh button as pressable.
    pub fn can_refresh(&self) -> bool {
        !matches!(self.status, BriefStatus::Loading | BriefStatus::NoApiKey)
    }
}

/// A registered card.
///
/// `Rc` rather than `Box` so notification can take a snapshot of the list and
/// drop the borrow before calling anything — see [`BriefStore::set`].
type Observer = Rc<dyn Fn(&BriefState)>;

pub struct BriefStore {
    state: RefCell<BriefState>,
    observers: RefCell<Vec<Observer>>,
    pool: SqlitePool,
    rt: tokio::runtime::Handle,
    athlete: Rc<RefCell<AthleteProfile>>,
}

impl BriefStore {
    pub fn new(
        pool: SqlitePool,
        rt: tokio::runtime::Handle,
        athlete: Rc<RefCell<AthleteProfile>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(BriefState::default()),
            observers: RefCell::new(Vec::new()),
            pool,
            rt,
            athlete,
        })
    }

    /// The state as it stands.
    ///
    /// For a card that needs to re-render for a reason of its own — the
    /// Coaching page redraws when its workout list reloads — rather than
    /// because the brief changed.
    pub fn state(&self) -> BriefState {
        self.state.borrow().clone()
    }

    /// Register a card, and tell it the current state straight away.
    ///
    /// The immediate call is what lets a page built after the brief arrived —
    /// or rebuilt on navigation — render without waiting for the next change.
    pub fn observe(self: &Rc<Self>, f: impl Fn(&BriefState) + 'static) {
        let observer: Observer = Rc::new(f);
        observer(&self.state.borrow().clone());
        self.observers.borrow_mut().push(observer);
    }

    /// Read what is cached, and write today's brief if there is none.
    ///
    /// Call once, after every page has been built, so no card misses the
    /// result. This is the only automatic request the app makes.
    pub fn start(self: &Rc<Self>) {
        let store = Rc::clone(self);
        let pool = self.pool.clone();
        let athlete = self.athlete.borrow().clone();
        let today = chrono::Local::now().date_naive();
        let has_key = api_key().is_some();

        spawn_to_main(
            &self.rt,
            async move {
                let cached = ai_cache::daily_brief(&pool).await;
                let fingerprint = brief::input::current_fingerprint(&pool, &athlete, today).await;
                (cached, fingerprint)
            },
            move |(cached, fingerprint)| {
                // A failed read does nothing at all. Falling through to
                // generating would bill the rider because SQLite hiccupped, and
                // overwrite the good brief already cached for today.
                let cached = match cached {
                    Ok(brief) => brief,
                    Err(e) => {
                        tracing::error!("Could not read the cached brief: {e}");
                        return;
                    }
                };
                // A fingerprint that would not compute is not evidence the
                // brief is stale, so compare against nothing and show it.
                let fingerprint = fingerprint.unwrap_or_else(|e| {
                    tracing::error!("Could not fingerprint the brief's inputs: {e}");
                    String::new()
                });

                let today_str = today.format("%Y-%m-%d").to_string();
                match brief::startup_action(cached.as_ref(), &today_str, &fingerprint, has_key) {
                    StartupAction::Show => store.set(BriefState {
                        brief: cached,
                        status: BriefStatus::Idle,
                    }),
                    StartupAction::ShowOutOfDate => store.set(BriefState {
                        brief: cached,
                        status: BriefStatus::OutOfDate,
                    }),
                    StartupAction::NeedKey => store.set(BriefState {
                        brief: cached,
                        status: BriefStatus::NoApiKey,
                    }),
                    StartupAction::Generate => store.generate(),
                }
            },
        );
    }

    /// The rider pressed Refresh. Always asks, always bills.
    pub fn refresh(self: &Rc<Self>) {
        self.generate();
    }

    /// Re-read the inputs and mark the brief out of date if they have moved.
    ///
    /// Runs on every page navigation. It must never generate: doing so would
    /// bill the rider for walking around their own app.
    pub fn revalidate(self: &Rc<Self>) {
        // Nothing to compare against, or something already in flight.
        if self.state.borrow().brief.is_none() || self.state.borrow().status == BriefStatus::Loading
        {
            return;
        }

        let store = Rc::clone(self);
        let pool = self.pool.clone();
        let athlete = self.athlete.borrow().clone();
        let today = chrono::Local::now().date_naive();

        spawn_to_main(
            &self.rt,
            async move { brief::input::current_fingerprint(&pool, &athlete, today).await },
            move |fingerprint| {
                let Ok(fingerprint) = fingerprint else {
                    // Say nothing rather than claim staleness we cannot show.
                    return;
                };
                let current = store.state.borrow().clone();
                let Some(brief) = &current.brief else { return };

                let today_str = today.format("%Y-%m-%d").to_string();
                let overtaken = !brief.is_for(&today_str) || brief.is_stale_for(&fingerprint);

                // Only ever moves between Idle and OutOfDate. A failure or a
                // missing key is a more important thing to be showing.
                match (overtaken, current.status) {
                    (true, BriefStatus::Idle) => store.set(BriefState {
                        status: BriefStatus::OutOfDate,
                        ..current
                    }),
                    (false, BriefStatus::OutOfDate) => store.set(BriefState {
                        status: BriefStatus::Idle,
                        ..current
                    }),
                    _ => {}
                }
            },
        );
    }

    /// Ask for a brief. The one place a request is actually made.
    fn generate(self: &Rc<Self>) {
        let Some(api_key) = api_key() else {
            let brief = self.state.borrow().brief.clone();
            self.set(BriefState {
                brief,
                status: BriefStatus::NoApiKey,
            });
            return;
        };

        let previous = self.state.borrow().brief.clone();
        self.set(BriefState {
            brief: previous.clone(),
            status: BriefStatus::Loading,
        });

        let store = Rc::clone(self);
        let pool = self.pool.clone();
        // Read now rather than at construction: the brief is written against
        // the rider's current FTP and heart-rate range.
        let athlete = self.athlete.borrow().clone();
        let today = chrono::Local::now().date_naive();

        spawn_to_main(
            &self.rt,
            async move {
                // Freshen the local copy first — the brief should be written
                // about the rides the rider actually did. This also means the
                // fingerprint taken inside `generate` covers what just arrived.
                let (icu_id, icu_key) = intervals_credentials(&pool).await;
                crate::ai::intervals::sync_recent(&pool, &icu_id, &icu_key, today).await;

                brief::generate(&pool, &api_key, athlete, today).await
            },
            move |result| match result {
                Ok(brief) => store.set(BriefState {
                    brief: Some(brief),
                    status: BriefStatus::Idle,
                }),
                Err(e) => store.set(BriefState {
                    // Keep whatever was on screen. A failed refresh should not
                    // cost the rider the brief they already had.
                    brief: previous,
                    status: BriefStatus::Failed(e.into()),
                }),
            },
        );
    }

    /// Replace the state and tell every card.
    fn set(self: &Rc<Self>, next: BriefState) {
        *self.state.borrow_mut() = next;

        // Snapshot both, and hold no borrow while the observers run: they
        // update widgets, which can re-enter this store (CLAUDE.md §2.4).
        let snapshot = self.state.borrow().clone();
        let observers: Vec<Observer> = self.observers.borrow().clone();
        for observer in observers {
            observer(&snapshot);
        }
    }
}

/// The Anthropic key, if one is stored and not blank.
fn api_key() -> Option<String> {
    match keystore::get_secret(keystore::KEY_ANTHROPIC) {
        Ok(Some(key)) if !key.trim().is_empty() => Some(key),
        _ => None,
    }
}

/// The Intervals.icu athlete id and key, either possibly empty.
async fn intervals_credentials(pool: &SqlitePool) -> (String, String) {
    let id = crate::data::settings::load_intervals(pool)
        .await
        .map(|s| s.athlete_id)
        .unwrap_or_default();
    let key = keystore::get_secret(keystore::KEY_INTERVALS_API)
        .unwrap_or(None)
        .unwrap_or_default();
    (id, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: BriefStatus, has_brief: bool) -> BriefState {
        BriefState {
            brief: has_brief.then(DailyBrief::default),
            status,
        }
    }

    #[test]
    fn should_describe_every_state_in_one_sentence() {
        // Three cards read this. They must not each invent their own wording.
        assert_eq!(
            state(BriefStatus::Idle, true).provenance(),
            "From this morning's brief"
        );
        assert_eq!(state(BriefStatus::Idle, false).provenance(), "No brief yet");
        assert_eq!(
            state(BriefStatus::Loading, false).provenance(),
            "Asking your coach…"
        );
        assert_eq!(
            state(BriefStatus::OutOfDate, true).provenance(),
            "Out of date — your training has changed since"
        );
        assert_eq!(
            state(BriefStatus::NoApiKey, false).provenance(),
            "No AI provider key configured"
        );
        assert_eq!(
            state(BriefStatus::Failed(AiFailure::Request), true).provenance(),
            "Could not reach your coach"
        );
    }

    #[test]
    fn should_not_offer_refresh_while_a_request_is_out() {
        assert!(!state(BriefStatus::Loading, false).can_refresh());
    }

    #[test]
    fn should_not_offer_refresh_without_a_key_to_ask_with() {
        // Pressing it could only fail, and the banner already says why.
        assert!(!state(BriefStatus::NoApiKey, false).can_refresh());
    }

    #[test]
    fn should_offer_refresh_when_the_brief_is_out_of_date_or_failed() {
        assert!(state(BriefStatus::OutOfDate, true).can_refresh());
        assert!(state(BriefStatus::Failed(AiFailure::Request), true).can_refresh());
        assert!(state(BriefStatus::Idle, true).can_refresh());
    }
}
