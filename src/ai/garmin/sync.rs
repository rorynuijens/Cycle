//! Garmin Connect as the source of activities, activity files and wellness.
//!
//! Everything lands in the tables the Intervals.icu sync fills, through the
//! same store functions, so the rest of the app reads a Garmin ride exactly as
//! it reads one from there. Ids are prefixed `g` so the two can never collide
//! ([`crate::data::dedupe::is_garmin_id`]). Intervals.icu stays for the workout library and
//! the optional upload; when a Garmin key is set, it no longer syncs activities.
//!
//! Garmin gives a load (TSS) only for activities with power, and scores it
//! against whatever FTP the head unit held, so the load is worked out here
//! instead, when an activity is stored: from power for a ride with power,
//! against the FTP [`Thresholds::ftp_on`] picks; from heart rate otherwise,
//! once the activity's file has been downloaded ([`fetch_files`]).
//!
//! Every fetch goes through a [`Source`], so the whole sync runs in the tests
//! against a fake one.

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{icu_sport_type, AccessToken, GarminActivity};
use crate::ai::intervals::{self, ActivitySummary, WellnessEntry};
use crate::data::{db, settings, sport};
use crate::training::load::{hrss, power_load, HrThresholds};

/// How far back a routine sync pulls activities — as for Intervals.icu.
const SYNC_ACTIVITY_DAYS: i64 = 30;
/// How far back a routine sync pulls wellness — as for Intervals.icu.
const SYNC_WELLNESS_DAYS: i64 = 7;
/// How many activity files one routine pass downloads. Each is one request;
/// ten keeps a pass well inside Garmin's 100 a minute.
const FILE_BATCH: usize = 10;
/// The pause between two downloads of a history import, which keeps it near
/// Garmin's rate limit rather than running into it on every file.
const IMPORT_DOWNLOAD_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// The FTPs Garmin scores a ride against when none was ever set: 200 W, give
/// or take the rounding of normalised power over intensity factor.
const UNSET_FTP: std::ops::RangeInclusive<u32> = 198..=202;
/// How far from a ride the FTP of another ride may be borrowed.
const NEARBY_FTP_DAYS: i64 = 45;
/// Below this a stored resting heart rate is a sensor fault, not a reading.
const MIN_RESTING_HR: u32 = 26;
/// How long before the key expires the rider is warned.
const EXPIRY_WARNING_DAYS: i64 = 7;

// ── Source ───────────────────────────────────────────────────────────────────

/// Where a sync reads from: the live API ([`Live`]), or a fake in the tests.
pub trait Source {
    /// Every activity that started between two dates, inclusive.
    fn activities(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<GarminActivity>>> + Send;
    /// One entry per day between two dates, inclusive.
    fn wellness(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<WellnessEntry>>> + Send;
    /// One activity's FIT file, by Garmin's own id (no `g`).
    fn fit(&self, garmin_id: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;
    /// The day of the first activity on the account.
    fn earliest(&self) -> impl Future<Output = Result<Option<NaiveDate>>> + Send;
}

/// The Garmin Connect API, signed in with the rider's personal key.
pub struct Live {
    token: AccessToken,
}

impl Live {
    /// Exchange the key for a token. Fails — with a 401 that [`super::status_of`]
    /// reads — when Garmin no longer accepts the key.
    pub async fn connect(api_key: &str) -> Result<Self> {
        Ok(Self {
            token: super::fetch_token(api_key).await?,
        })
    }
}

impl Source for Live {
    fn activities(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<GarminActivity>>> + Send {
        super::fetch_activities(&self.token, oldest, newest)
    }

    fn wellness(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<WellnessEntry>>> + Send {
        super::fetch_wellness(&self.token, oldest, newest)
    }

    fn fit(&self, garmin_id: &str) -> impl Future<Output = Result<Vec<u8>>> + Send {
        super::fetch_activity_fit(&self.token, garmin_id)
    }

    fn earliest(&self) -> impl Future<Output = Result<Option<NaiveDate>>> + Send {
        super::fetch_earliest_activity(&self.token)
    }
}

/// The rider's Garmin key, or `None` when none is set. Blocking: the keyring
/// is a D-Bus call, so call it from `spawn_blocking`, never the GTK thread.
pub fn api_key() -> Option<String> {
    crate::data::keystore::get_secret(crate::data::keystore::KEY_GARMIN_API)
        .unwrap_or(None)
        .filter(|k| !k.trim().is_empty())
}

/// Whether a failure means Garmin no longer accepts the key.
pub fn is_unauthorized(err: &anyhow::Error) -> bool {
    matches!(super::status_of(err), Some(401 | 403))
}

// ── Thresholds ───────────────────────────────────────────────────────────────

/// What a Garmin activity's load is scored against.
#[derive(Debug, Clone)]
pub struct Thresholds {
    /// The app's own FTP history, `(day, watts)`, oldest first.
    ftp_history: Vec<(NaiveDate, u32)>,
    /// The FTPs Garmin scored rides against, `(day, watts)`, oldest first,
    /// with its unset default ([`UNSET_FTP`]) left out.
    set_ftps: Vec<(NaiveDate, u32)>,
    athlete_ftp: u32,
    lthr: u32,
    max_hr: u32,
    resting_hr: u32,
}

impl Thresholds {
    /// The rider's thresholds as stored, with the set FTPs of `activities` to
    /// fall back on for days before the app's own FTP history begins.
    pub async fn load(pool: &sqlx::SqlitePool, activities: &[GarminActivity]) -> Result<Self> {
        let athlete = db::load_or_create_athlete(pool).await?;
        let lthr = settings::lthr(pool)
            .await?
            .unwrap_or_else(|| settings::default_lthr(athlete.max_hr));
        Ok(Self::new(
            db::load_ftp_history(pool).await?,
            activities,
            athlete.ftp_watts,
            lthr,
            athlete.max_hr,
            athlete.resting_hr,
        ))
    }

    fn new(
        ftp_history: Vec<(NaiveDate, u32)>,
        activities: &[GarminActivity],
        athlete_ftp: u32,
        lthr: u32,
        max_hr: u32,
        resting_hr: u32,
    ) -> Self {
        let mut set_ftps: Vec<(NaiveDate, u32)> = activities
            .iter()
            .filter_map(|a| Some((a.summary.start_date_local, usable_set_ftp(a.set_ftp)?)))
            .collect();
        set_ftps.sort();
        Self {
            ftp_history,
            set_ftps,
            athlete_ftp,
            lthr,
            max_hr,
            resting_hr,
        }
    }

    /// The FTP a ride on `day` is scored against, in the rider's order:
    ///
    /// 1. the app's own FTP history, from its first entry on — the last FTP
    ///    on or before the day;
    /// 2. before that, the FTP the head unit scored this ride against (`own`);
    /// 3. else the median of the FTPs Garmin scored rides within
    ///    [`NEARBY_FTP_DAYS`] against;
    /// 4. else the athlete's FTP.
    ///
    /// A Garmin FTP of 200 W is its default when none was set, so it counts
    /// as no FTP at all in 2 and 3.
    pub fn ftp_on(&self, day: NaiveDate, own: Option<u32>) -> u32 {
        if let Some(&(first, _)) = self.ftp_history.first() {
            if day >= first {
                return self
                    .ftp_history
                    .iter()
                    .rev()
                    .find(|(d, _)| *d <= day)
                    .map_or(self.athlete_ftp, |(_, w)| *w);
            }
        }
        if let Some(ftp) = usable_set_ftp(own) {
            return ftp;
        }
        let near: Vec<u32> = self
            .set_ftps
            .iter()
            .filter(|(d, _)| (*d - day).num_days().abs() <= NEARBY_FTP_DAYS)
            .map(|(_, w)| *w)
            .collect();
        median(near).unwrap_or(self.athlete_ftp)
    }

    /// The power load of a ride with power; `None` for anything else, which
    /// is scored from heart rate once its file arrives.
    fn power_load(&self, a: &GarminActivity, icu_sport: &str) -> Option<f32> {
        if !sport::is_cycling(icu_sport) {
            return None;
        }
        let np = a.summary.normalized_watts.filter(|w| *w > 0)?;
        let moving = a.summary.moving_time.filter(|s| *s > 0)?;
        let ftp = self.ftp_on(a.summary.start_date_local, a.set_ftp);
        power_load(np as f32, moving, ftp)
    }

    /// The heart rates an activity is scored against, with that morning's
    /// resting heart rate when there is one.
    fn hr(&self, resting_that_day: Option<u32>) -> HrThresholds {
        HrThresholds {
            resting: resting_that_day
                .filter(|r| *r >= MIN_RESTING_HR)
                .unwrap_or(self.resting_hr),
            lthr: self.lthr,
            max: self.max_hr,
        }
    }
}

fn usable_set_ftp(ftp: Option<u32>) -> Option<u32> {
    ftp.filter(|f| *f > 0 && !UNSET_FTP.contains(f))
}

/// The middle value; of two middles, the upper.
fn median(mut values: Vec<u32>) -> Option<u32> {
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

// ── Storing ──────────────────────────────────────────────────────────────────

/// A Garmin activity in the shape the activity table stores, with its id
/// prefixed, its sport spelled as Intervals.icu spells it, and its power load.
fn to_summary(a: &GarminActivity, t: &Thresholds) -> ActivitySummary {
    let mut s = a.summary.clone();
    s.id = format!("g{}", s.id);
    s.sport_type = icu_sport_type(&s.sport_type);
    s.icu_training_load = t.power_load(a, &s.sport_type);
    s
}

/// Store activities, fold away the Intervals.icu copies of any of them, and
/// return how many were stored.
///
/// The store links an activity to a ride recorded here, so a ride the rider
/// uploaded to Garmin Connect by hand comes back as that ride, not a second
/// one. The fold runs after, so a ride whose link was to the Intervals.icu
/// copy moves to the Garmin one.
async fn store(pool: &sqlx::SqlitePool, activities: &[GarminActivity]) -> Result<(usize, usize)> {
    let t = Thresholds::load(pool, activities).await?;
    let summaries = activities.iter().map(|a| to_summary(a, &t)).collect();
    let saved = intervals::store_activities(pool, summaries).await.saved;
    let merged = db::merge_twins(pool).await?;
    Ok((saved, merged))
}

/// What one sync brought in.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub activities: usize,
    pub wellness: usize,
    /// Intervals.icu rows folded into their Garmin copies.
    pub merged: usize,
}

/// Pull activities from the last `activity_days` and wellness from the last
/// `wellness_days`.
///
/// Wellness goes first, so that a heart-rate load worked out later has the
/// morning's resting heart rate to use; its failure is logged, not returned —
/// the activities are the point. Files are left to [`fetch_files`], which
/// takes far longer and which nothing waits on.
pub async fn sync<S: Source>(
    pool: &sqlx::SqlitePool,
    source: &S,
    today: NaiveDate,
    activity_days: i64,
    wellness_days: i64,
) -> Result<Report> {
    let mut report = Report::default();
    match source
        .wellness(today - Duration::days(wellness_days), today)
        .await
    {
        Ok(entries) => report.wellness = intervals::store_wellness(pool, entries).await,
        Err(e) if is_unauthorized(&e) => return Err(e),
        Err(e) => tracing::warn!("Garmin wellness sync failed: {e:#}"),
    }
    let activities = source
        .activities(today - Duration::days(activity_days), today)
        .await?;
    (report.activities, report.merged) = store(pool, &activities).await?;
    Ok(report)
}

/// The routine sync run before the morning brief: the last month of
/// activities, the last week of wellness.
pub async fn sync_recent<S: Source>(
    pool: &sqlx::SqlitePool,
    source: &S,
    today: NaiveDate,
) -> Result<Report> {
    sync(pool, source, today, SYNC_ACTIVITY_DAYS, SYNC_WELLNESS_DAYS).await
}

/// The sync run before the morning brief, with the file pass left running
/// behind it. Never fails, for the reason [`intervals::sync_recent`] never
/// does: a brief from yesterday's data beats none. Returns what the rider
/// should be told — a refused key, or one about to expire.
pub async fn routine(pool: &sqlx::SqlitePool, api_key: &str, today: NaiveDate) -> Option<String> {
    routine_with(pool, Live::connect(api_key).await, today).await
}

/// [`routine`] once signed in — or not: `connected` is the sign-in's outcome,
/// so the tests can hand in a fake source, or a refusal.
async fn routine_with<S: Source + Send + Sync + 'static>(
    pool: &sqlx::SqlitePool,
    connected: Result<S>,
    today: NaiveDate,
) -> Option<String> {
    let expiry = match settings::garmin_key_expires(pool).await {
        Ok(expires) => expiry_notice(expires, today),
        Err(e) => {
            tracing::error!("Could not read the Garmin key expiry: {e}");
            None
        }
    };
    let source = match connected {
        Ok(source) => std::sync::Arc::new(source),
        Err(e) if is_unauthorized(&e) => return Some(UNAUTHORIZED_NOTICE.to_string()),
        Err(e) => {
            tracing::warn!("Garmin sync could not sign in: {e:#}");
            return expiry;
        }
    };
    match sync_recent(pool, &*source, today).await {
        Ok(r) => tracing::info!(
            "Garmin sync stored {} activities, {} wellness days",
            r.activities,
            r.wellness
        ),
        Err(e) if is_unauthorized(&e) => return Some(UNAUTHORIZED_NOTICE.to_string()),
        Err(e) => tracing::warn!("Garmin sync failed: {e:#}"),
    }
    // Files are for the Fitness page and heart-rate loads, not the brief, and
    // can run to megabytes — fetched alongside, never in front.
    tokio::spawn({
        let pool = pool.clone();
        async move {
            match fetch_recent_files(&pool, &*source).await {
                Ok(r) if r.unauthorized => tracing::warn!("Garmin refused the key mid-pass"),
                Ok(_) => {}
                Err(e) => tracing::error!("Garmin file pass failed: {e:#}"),
            }
        }
    });
    expiry
}

// ── Files ────────────────────────────────────────────────────────────────────

/// What one pass over the missing activity files did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FilesReport {
    /// Files downloaded and cached.
    pub fetched: usize,
    /// Activities given a heart-rate load from their file.
    pub scored: usize,
    /// Files Garmin no longer has, or that would not read; cached as `[]` so
    /// they are not asked for again.
    pub missing: usize,
    /// Whether the pass stopped because Garmin refused the key.
    pub unauthorized: bool,
    /// Whether the pass stopped on any other failure, files left unfetched.
    pub stopped: bool,
}

/// Download the files of activities that have none cached, newest first, at
/// most `limit` (all when `None`), and score each that has no load yet from its
/// heart rate. `progress` hears `(done, total)` before each file.
///
/// A file Garmin answers with 404 — it has the activity but lost the upload —
/// or one that does not parse is cached as `[]`: neither will change by asking
/// again. A refused key ends the pass; so does any other failure, leaving that
/// activity uncached for next time.
pub async fn fetch_files<S: Source>(
    pool: &sqlx::SqlitePool,
    source: &S,
    limit: Option<usize>,
    pause: std::time::Duration,
    progress: impl Fn(usize, usize),
) -> Result<FilesReport> {
    let queue = db::garmin_activities_missing_files(pool, limit).await?;
    let t = Thresholds::load(pool, &[]).await?;
    let mut report = FilesReport::default();
    for (n, file) in queue.iter().enumerate() {
        progress(n, queue.len());
        if n > 0 && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        let garmin_id = file.id.trim_start_matches('g');
        let bytes = match source.fit(garmin_id).await {
            Ok(bytes) => bytes,
            Err(e) if super::status_of(&e) == Some(404) => {
                tracing::warn!("Garmin has no file for activity {}", file.id);
                db::save_activity_streams(pool, &file.id, "[]").await?;
                report.missing += 1;
                continue;
            }
            Err(e) if is_unauthorized(&e) => {
                report.unauthorized = true;
                break;
            }
            Err(e) => {
                tracing::warn!("Garmin file download stopped: {e:#}");
                report.stopped = true;
                break;
            }
        };
        let points = match crate::data::fit::import_fit_bytes(&bytes) {
            Ok(session) => session.data_points,
            Err(e) => {
                tracing::warn!("Garmin file for {} would not read: {e:#}", file.id);
                db::save_activity_streams(pool, &file.id, "[]").await?;
                report.missing += 1;
                continue;
            }
        };
        let json = crate::data::streams::streams_json_from_points(&points);
        db::save_activity_streams(pool, &file.id, &json).await?;
        report.fetched += 1;
        if !file.has_load {
            let resting = resting_hr_on(pool, file.date).await?;
            if let Some(load) = hrss(&points, t.hr(resting)) {
                db::set_activity_tss(pool, &file.id, load).await?;
                report.scored += 1;
            }
        }
    }
    progress(queue.len(), queue.len());
    if report.fetched > 0 {
        tracing::info!("Garmin file pass cached {} activities", report.fetched);
    }
    Ok(report)
}

/// The routine file pass: [`FILE_BATCH`] at a time, after the sync.
pub async fn fetch_recent_files<S: Source>(
    pool: &sqlx::SqlitePool,
    source: &S,
) -> Result<FilesReport> {
    fetch_files(
        pool,
        source,
        Some(FILE_BATCH),
        std::time::Duration::ZERO,
        |_, _| {},
    )
    .await
}

async fn resting_hr_on(pool: &sqlx::SqlitePool, day: NaiveDate) -> Result<Option<u32>> {
    let day = day.format("%Y-%m-%d").to_string();
    Ok(db::load_wellness_between(pool, &day, &day)
        .await?
        .first()
        .and_then(|w| w.resting_hr))
}

/// One activity's file as the streams the cache holds, for the calendar's
/// Refresh. `garmin_id` is the stored id, with its `g`.
pub async fn fetch_streams_json(api_key: &str, garmin_id: &str) -> Result<String> {
    let source = Live::connect(api_key).await?;
    let bytes = source.fit(garmin_id.trim_start_matches('g')).await?;
    let session = crate::data::fit::import_fit_bytes(&bytes)
        .context("Garmin's file for this activity would not read")?;
    Ok(crate::data::streams::streams_json_from_points(
        &session.data_points,
    ))
}

// ── History import ───────────────────────────────────────────────────────────

/// Set while a history import runs, so a second click cannot start another.
static IMPORTING: AtomicBool = AtomicBool::new(false);

/// Holds the import slot; dropping it frees the slot, however the import ends.
pub struct ImportGuard {
    /// False only for the stand-in the tests use, which must not free a slot
    /// it never took: the flag is shared by every test in the process.
    held: bool,
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        if self.held {
            IMPORTING.store(false, Ordering::SeqCst);
        }
    }
}

/// Take the import slot, or `None` when an import already holds it.
pub fn try_begin_import() -> Option<ImportGuard> {
    IMPORTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| ImportGuard { held: true })
}

/// Where a history import has got to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    Wellness { done: usize, total: usize },
    Activities,
    Files { done: usize, total: usize },
}

/// What a history import brought in.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ImportReport {
    pub activities: usize,
    pub wellness: usize,
    pub merged: usize,
    /// Rides recorded here newly linked to their Garmin activity.
    pub linked: usize,
    pub files: FilesReport,
}

/// Bring in the rider's whole Garmin history: every day of wellness, every
/// activity, and every activity's file.
///
/// Resumable: everything is upserted, and a file already cached is not
/// downloaded again, so an import cut short — closed app, lost network, a
/// refused key — picks up at the first file it did not get. `_guard` proves
/// the caller holds the import slot ([`try_begin_import`]).
pub async fn import_history<S: Source>(
    pool: &sqlx::SqlitePool,
    source: &S,
    today: NaiveDate,
    pause: std::time::Duration,
    _guard: &ImportGuard,
    progress: impl Fn(Progress),
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let Some(first) = source.earliest().await? else {
        return Ok(report);
    };

    let chunks = super::chunks(first, today);
    for (n, (from, to)) in chunks.iter().enumerate() {
        progress(Progress::Wellness {
            done: n,
            total: chunks.len(),
        });
        match source.wellness(*from, *to).await {
            Ok(entries) => report.wellness += intervals::store_wellness(pool, entries).await,
            Err(e) if is_unauthorized(&e) => return Err(e),
            // One month's wellness lost is a gap in a chart; it is not worth
            // the whole import, and a re-run fills it.
            Err(e) => tracing::warn!("Garmin wellness {from} → {to} failed: {e:#}"),
        }
    }

    progress(Progress::Activities);
    let activities = source.activities(first, today).await?;
    (report.activities, report.merged) = store(pool, &activities).await?;
    // The routine link covers the last two months; this history goes back years.
    report.linked = db::reconcile_icu_links_within(pool, None).await?;

    report.files = fetch_files(pool, source, None, pause, |done, total| {
        progress(Progress::Files { done, total })
    })
    .await?;
    Ok(report)
}

/// [`import_history`] against the live API, with the pause that keeps it
/// inside Garmin's rate limit.
pub async fn import_history_live(
    pool: &sqlx::SqlitePool,
    api_key: &str,
    today: NaiveDate,
    guard: &ImportGuard,
    progress: impl Fn(Progress),
) -> Result<ImportReport> {
    let source = Live::connect(api_key).await?;
    import_history(pool, &source, today, IMPORT_DOWNLOAD_PAUSE, guard, progress).await
}

// ── What the rider is told ───────────────────────────────────────────────────

/// What the rider is told when Garmin refuses the key.
pub const UNAUTHORIZED_NOTICE: &str =
    "Garmin refused your API key — it may have expired. Make a new one in Garmin Connect";

/// A warning when the key expires within [`EXPIRY_WARNING_DAYS`], or already
/// has; nothing when it is further off, or the rider has not said when.
pub fn expiry_notice(expires: Option<NaiveDate>, today: NaiveDate) -> Option<String> {
    let expires = expires?;
    Some(match (expires - today).num_days() {
        d if d > EXPIRY_WARNING_DAYS => return None,
        d if d < 0 => format!(
            "Your Garmin API key expired on {} — make a new one in Garmin Connect",
            expires.format("%-d %B")
        ),
        0 => "Your Garmin API key expires today — make a new one in Garmin Connect".to_string(),
        1 => "Your Garmin API key expires tomorrow — make a new one in Garmin Connect".to_string(),
        d => format!("Your Garmin API key expires in {d} days — make a new one in Garmin Connect"),
    })
}

/// The suffix a manual sync's toast adds for Intervals.icu copies folded away.
pub fn merged_suffix(merged: usize) -> String {
    match merged {
        0 => String::new(),
        1 => " — 1 Intervals.icu copy folded in".to_string(),
        n => format!(" — {n} Intervals.icu copies folded in"),
    }
}

#[cfg(test)]
mod tests;
