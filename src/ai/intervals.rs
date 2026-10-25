use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveDateTime};
use serde::Deserialize;
use serde_json;

use crate::data::streams::ActivityStreams;

const BASE_URL: &str = "https://intervals.icu/api/v1";

fn make_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

// ── Activities ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ActivitySummary {
    pub id: String,
    pub name: String,
    pub start_date_local: NaiveDate,
    pub start_datetime_local: Option<NaiveDateTime>,
    pub icu_training_load: Option<f32>,
    pub moving_time: Option<u32>,
    pub average_watts: Option<u32>,
    pub normalized_watts: Option<u32>,
    pub average_hr: Option<u32>,
    pub max_hr: Option<u32>,
    pub sport_type: String,
    pub distance_m: Option<f32>,
    pub elevation_gain_m: Option<f32>,
    pub average_cadence: Option<f32>,
}

impl ActivitySummary {
    /// Whether Intervals.icu sent this activity stripped to its id and date —
    /// what it does with one it may not pass on (Strava's today, Garmin's
    /// likely soon).
    ///
    /// Every recorded or hand-entered activity has a moving time, so a copy
    /// with no time and no figure of any kind is withheld, not merely empty.
    pub fn is_withheld(&self) -> bool {
        self.moving_time.is_none()
            && self.icu_training_load.is_none()
            && self.average_watts.is_none()
            && self.normalized_watts.is_none()
            && self.average_hr.is_none()
            && self.max_hr.is_none()
            && self.distance_m.is_none()
            && self.elevation_gain_m.is_none()
            && self.average_cadence.is_none()
    }
}

#[derive(Deserialize)]
struct RawActivity {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    start_date_local: String,
    #[serde(default)]
    icu_training_load: Option<f64>,
    #[serde(default)]
    moving_time: Option<u32>,
    #[serde(default)]
    icu_average_watts: Option<f64>,
    #[serde(default)]
    icu_weighted_avg_watts: Option<f64>,
    #[serde(default)]
    average_heartrate: Option<f64>,
    #[serde(default)]
    max_heartrate: Option<f64>,
    #[serde(rename = "type", default)]
    sport_type: Option<String>,
    #[serde(default)]
    distance: Option<f64>,
    #[serde(default)]
    total_elevation_gain: Option<f64>,
    #[serde(default)]
    average_cadence: Option<f64>,
}

/// Fetch activities between two dates (inclusive). `oldest` and `newest` are ISO date strings.
pub async fn fetch_activities(
    athlete_id: &str,
    api_key: &str,
    oldest: NaiveDate,
    newest: NaiveDate,
) -> Result<Vec<ActivitySummary>> {
    let client = make_client()?;
    let url = format!(
        "{BASE_URL}/athlete/{athlete_id}/activities\
         ?oldest={oldest}&newest={newest}\
         &fields=id,name,start_date_local,icu_training_load,moving_time,\
         icu_average_watts,icu_weighted_avg_watts,average_heartrate,max_heartrate,type,\
         distance,total_elevation_gain,average_cadence"
    );

    let response = client
        .get(&url)
        .basic_auth("API_KEY", Some(api_key))
        .send()
        .await
        .context("Intervals.icu activities request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Intervals.icu API error {status}: {body}");
    }

    let text = response
        .text()
        .await
        .context("failed to read activities response")?;

    let raw: Vec<RawActivity> =
        serde_json::from_str(&text).context("failed to parse activities response")?;

    Ok(raw
        .into_iter()
        .filter_map(|r| {
            // start_date_local can be "2024-01-15T08:30:00" or "2024-01-15"
            let datetime =
                NaiveDateTime::parse_from_str(&r.start_date_local, "%Y-%m-%dT%H:%M:%S").ok();
            let date = datetime.map(|dt| dt.date()).or_else(|| {
                NaiveDate::parse_from_str(r.start_date_local.get(..10)?, "%Y-%m-%d").ok()
            })?;
            Some(ActivitySummary {
                id: r.id,
                name: r.name,
                start_date_local: date,
                start_datetime_local: datetime,
                icu_training_load: r.icu_training_load.map(|v| v as f32),
                moving_time: r.moving_time,
                average_watts: r.icu_average_watts.map(|v| v.max(0.0).round() as u32),
                normalized_watts: r.icu_weighted_avg_watts.map(|v| v.max(0.0).round() as u32),
                average_hr: r.average_heartrate.map(|v| v.max(0.0).round() as u32),
                max_hr: r.max_heartrate.map(|v| v.max(0.0).round() as u32),
                sport_type: r.sport_type.unwrap_or_default(),
                distance_m: r.distance.map(|v| v as f32),
                elevation_gain_m: r.total_elevation_gain.map(|v| v as f32),
                average_cadence: r.average_cadence.map(|v| v as f32),
            })
        })
        .collect())
}

// ── Wellness ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WellnessEntry {
    pub date: NaiveDate,
    pub hrv: Option<f32>,
    pub resting_hr: Option<u32>,
    pub sleep_secs: Option<u32>,
    pub sleep_score: Option<u32>,
    pub steps: Option<u32>,
    pub calories: Option<u32>,
}

#[derive(Deserialize)]
struct RawWellness {
    id: String,
    #[serde(default)]
    hrv: Option<f64>,
    #[serde(rename = "restingHR", default)]
    resting_hr: Option<f64>,
    #[serde(rename = "sleepSecs", default)]
    sleep_secs: Option<i64>,
    #[serde(rename = "sleepScore", default)]
    sleep_score: Option<f64>,
    #[serde(default)]
    steps: Option<i64>,
    #[serde(default)]
    calories: Option<f64>,
}

/// Fetch wellness entries between two dates (inclusive).
pub async fn fetch_wellness(
    athlete_id: &str,
    api_key: &str,
    oldest: NaiveDate,
    newest: NaiveDate,
) -> Result<Vec<WellnessEntry>> {
    let client = make_client()?;
    let url = format!("{BASE_URL}/athlete/{athlete_id}/wellness?oldest={oldest}&newest={newest}");
    let response = client
        .get(&url)
        .basic_auth("API_KEY", Some(api_key))
        .send()
        .await
        .context("Intervals.icu wellness request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Intervals.icu wellness API error {status}: {body}");
    }
    let raw: Vec<RawWellness> = response
        .json()
        .await
        .context("failed to parse wellness response")?;
    Ok(raw
        .into_iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(&r.id, "%Y-%m-%d").ok()?;
            Some(WellnessEntry {
                date,
                hrv: r.hrv.map(|v| v as f32),
                resting_hr: r.resting_hr.map(|v| v.round() as u32),
                sleep_secs: r.sleep_secs.map(|v| v as u32),
                sleep_score: r.sleep_score.map(|v| v.round() as u32),
                steps: r.steps.map(|v| v as u32),
                calories: r.calories.map(|v| v.round() as u32),
            })
        })
        .collect())
}

// ── Workouts (library) ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WorkoutSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub target_duration: Option<u32>,
    pub icu_training_load: Option<f32>,
}

#[derive(Deserialize)]
struct RawWorkout {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    target_duration: Option<u32>,
    #[serde(default)]
    icu_training_load: Option<f64>,
}

/// Fetch workout templates from the athlete's Intervals.icu library.
pub async fn fetch_workouts(athlete_id: &str, api_key: &str) -> Result<Vec<WorkoutSummary>> {
    let client = make_client()?;
    let url = format!("{BASE_URL}/athlete/{athlete_id}/workouts");

    let response = client
        .get(&url)
        .basic_auth("API_KEY", Some(api_key))
        .send()
        .await
        .context("Intervals.icu workouts request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Intervals.icu API error {status}: {body}");
    }

    let raw: Vec<RawWorkout> = response
        .json()
        .await
        .context("failed to parse workouts response")?;

    Ok(raw
        .into_iter()
        .filter(|r| !r.name.trim().is_empty())
        .map(|r| WorkoutSummary {
            id: r.id,
            name: r.name,
            description: r.description.unwrap_or_default(),
            target_duration: r.target_duration,
            icu_training_load: r.icu_training_load.map(|v| v as f32),
        })
        .collect())
}

// ── Activity streams ──────────────────────────────────────────────────────────

/// The stream types the app reads — see [`ActivityStreams::from_json`].
///
/// Unfiltered, the endpoint also sends temperature, torque, pedal balance and
/// respiration: a fifth of a five-hour ride's megabyte, cached for nothing.
const STREAM_TYPES: &str = "time,watts,heartrate,cadence,distance,altitude,latlng,velocity_smooth";

/// Where an activity's streams live.
///
/// The path has no athlete prefix. Until 0.12.0 the app asked for
/// `/athlete/{id}/activities/{id}/streams`, which Intervals.icu does not
/// serve: every request 404'd, the 404 was read as "this ride has no streams",
/// and only the map endpoint's GPS track was ever cached.
fn streams_url(activity_id: &str) -> String {
    format!("{BASE_URL}/activity/{activity_id}/streams?types={STREAM_TYPES}")
}

/// What Intervals.icu answered when asked for one activity's streams.
#[derive(Debug, PartialEq)]
enum StreamFetch {
    /// The raw JSON (array-of-objects format), to be cached verbatim.
    Found(String),
    /// It has none — a manually entered ride, say.
    Missing,
    /// It has them but will not share them: the answer for an activity whose
    /// source forbids passing its data on.
    Withheld,
}

/// Which refusals mean something about the activity rather than the request.
///
/// Anything else — a bad key, a rate limit, a server fault — is an error, so a
/// passing failure is never mistaken for a ride without data.
fn stream_refusal(status: u16) -> Option<StreamFetch> {
    match status {
        404 => Some(StreamFetch::Missing),
        403 => Some(StreamFetch::Withheld),
        _ => None,
    }
}

/// Fetch per-second time-series streams for a single activity.
async fn fetch_activity_streams(api_key: &str, activity_id: &str) -> Result<StreamFetch> {
    let client = make_client()?;
    let url = streams_url(activity_id);

    let response = client
        .get(&url)
        .basic_auth("API_KEY", Some(api_key))
        .send()
        .await
        .context("Intervals.icu streams request failed")?;

    let status = response.status();
    tracing::debug!(url = %url, status = %status, "Streams endpoint response");

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if let Some(refusal) = stream_refusal(status.as_u16()) {
            tracing::debug!(body = %body, ?refusal, "Streams endpoint refused");
            return Ok(refusal);
        }
        anyhow::bail!("Intervals.icu streams API error {status}: {body}");
    }

    response
        .text()
        .await
        .map(StreamFetch::Found)
        .context("failed to read streams response")
}

/// One activity's streams, for the Garmin comparison harness only.
///
/// `None` covers both a ride without streams and one Intervals.icu withholds;
/// the comparison reports either as "no streams".
#[cfg(test)]
pub(super) async fn fetch_streams_for_comparison(
    api_key: &str,
    activity_id: &str,
) -> Result<Option<ActivityStreams>> {
    Ok(match fetch_activity_streams(api_key, activity_id).await? {
        StreamFetch::Found(json) => ActivityStreams::from_json(&json),
        StreamFetch::Missing | StreamFetch::Withheld => None,
    })
}

/// Fetch GPS coordinates for a single activity from the map endpoint.
///
/// Returns an empty vec (not an error) when the activity has no GPS track.
async fn fetch_activity_map(api_key: &str, activity_id: &str) -> Result<Vec<(f64, f64)>> {
    let client = make_client()?;
    // Map endpoint path is /api/v1/activity/{id}/map — no athlete prefix (per API docs).
    let url = format!("{BASE_URL}/activity/{activity_id}/map?weather=true");

    let response = client
        .get(&url)
        .basic_auth("API_KEY", Some(api_key))
        .send()
        .await
        .context("Intervals.icu map request failed")?;

    let status = response.status();

    tracing::debug!(url = %url, status = %status, "Map endpoint response");

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 404 {
            tracing::debug!(url = %url, body = %body, "Map endpoint 404 — treating as no GPS");
            return Ok(Vec::new());
        }
        anyhow::bail!("Intervals.icu map API error {status}: {body}");
    }

    let body = response
        .text()
        .await
        .context("failed to read map response")?;
    tracing::debug!(body_preview = %&body[..body.len().min(200)], "Map endpoint raw response");

    #[derive(Deserialize)]
    struct MapResponse {
        latlngs: Vec<[f64; 2]>,
    }

    let map: MapResponse = serde_json::from_str(&body).context("failed to parse map response")?;
    tracing::debug!("Map endpoint: {} GPS points", map.latlngs.len());
    Ok(map
        .latlngs
        .into_iter()
        .map(|[lat, lng]| (lat, lng))
        .collect())
}

/// Fetch streams and GPS for an activity, returning a single combined JSON string suitable for
/// caching in `activity_streams`.
///
/// The streams endpoint carries the GPS track itself. The map endpoint is only
/// asked when it does not — an activity with no streams can still have a
/// route — and its track is injected as a synthetic
/// `{"type":"latlng","data":[[lat,lng],...]}` entry so the existing
/// [`ActivityStreams`] parser handles everything in one pass. If neither
/// endpoint has anything, that is an error the caller shows.
pub async fn fetch_combined_activity_data(api_key: &str, activity_id: &str) -> Result<String> {
    let streams = match fetch_activity_streams(api_key, activity_id).await? {
        StreamFetch::Found(json) => Some(json),
        StreamFetch::Missing => None,
        StreamFetch::Withheld => anyhow::bail!(
            "Intervals.icu will not share this activity's data with other apps. \
             Uploading its FIT file to Intervals.icu yourself brings it back."
        ),
    };
    if let Some(json) = &streams {
        if ActivityStreams::from_json(json).is_some_and(|s| s.has_gps()) {
            return Ok(json.clone());
        }
    }

    let latlngs = fetch_activity_map(api_key, activity_id)
        .await
        .unwrap_or_default();
    tracing::debug!(
        has_streams = streams.is_some(),
        gps_points = latlngs.len(),
        "Activity data fetch complete"
    );
    let latlng_entry = || {
        let data: Vec<serde_json::Value> = latlngs
            .iter()
            .map(|(lat, lng)| serde_json::json!([lat, lng]))
            .collect();
        serde_json::json!({"type": "latlng", "data": data})
    };

    match (streams, latlngs.is_empty()) {
        (Some(json), true) => Ok(json),
        (Some(json), false) => {
            let mut arr: Vec<serde_json::Value> =
                serde_json::from_str(&json).context("failed to parse streams JSON")?;
            // Replace, never duplicate: the parser keeps whichever comes last.
            arr.retain(|s| s.get("type").and_then(|t| t.as_str()) != Some("latlng"));
            arr.push(latlng_entry());
            serde_json::to_string(&arr).context("failed to serialise combined activity data")
        }
        (None, false) => {
            serde_json::to_string(&[latlng_entry()]).context("failed to serialise GPS-only data")
        }
        (None, true) => anyhow::bail!(
            "Intervals.icu has no detailed data for this activity — it was probably \
             entered by hand rather than recorded."
        ),
    }
}

// ── Sync ──────────────────────────────────────────────────────────────────────

/// How far back a routine sync pulls activities.
const SYNC_ACTIVITY_DAYS: i64 = 30;

/// How far back a routine sync pulls wellness entries.
///
/// Shorter than the activity window because wellness is only ever read as "the
/// last week or so": it is a picture of the rider now, not of their season.
const SYNC_WELLNESS_DAYS: i64 = 7;

/// What [`store_activities`] did with one sync's worth of activities.
#[derive(Debug, Default, PartialEq)]
pub struct StoredActivities {
    pub saved: usize,
    /// Ids of the activities Intervals.icu sent without their data, newest
    /// first. None of them was written.
    pub withheld: Vec<String>,
}

/// Save activities fetched from Intervals.icu, and link any that are a ride
/// recorded here coming back after a round trip.
///
/// A withheld activity is skipped, not stored: the stripped copy has nothing to
/// add to a ride already held, and as a new row it would be a nameless ride
/// that never collapses with the full copy the rider uploads to bring it back.
pub async fn store_activities(
    pool: &sqlx::SqlitePool,
    activities: Vec<ActivitySummary>,
) -> StoredActivities {
    use crate::data::db;

    let mut saved = 0usize;
    let mut withheld = Vec::new();
    for a in activities {
        if a.is_withheld() {
            withheld.push((a.start_date_local, a.id));
            continue;
        }
        match db::upsert_intervals_activity(
            pool,
            &a.id,
            a.start_date_local,
            &a.name,
            a.icu_training_load,
            a.moving_time,
            a.average_watts,
            a.normalized_watts,
            a.average_hr,
            a.max_hr,
            &a.sport_type,
            a.start_datetime_local,
            a.distance_m,
            a.elevation_gain_m,
            a.average_cadence,
        )
        .await
        {
            Ok(()) => saved += 1,
            Err(e) => tracing::error!("upsert intervals activity: {e}"),
        }
    }
    // A ride recorded in-app can arrive back here after a round trip through
    // Garmin or Strava — link the two so it is shown and counted once.
    if let Err(e) = db::reconcile_icu_links(pool).await {
        tracing::error!("reconcile_icu_links: {e}");
    }
    if !withheld.is_empty() {
        tracing::warn!(
            "Intervals.icu withheld the data of {} activities",
            withheld.len()
        );
    }

    withheld.sort_by(|a, b| b.cmp(a));
    StoredActivities {
        saved,
        withheld: withheld.into_iter().map(|(_, id)| id).collect(),
    }
}

/// Save wellness entries, returning how many were written.
///
/// A day with no reading of any kind is skipped: it has nothing to merge into
/// what is held, and as a new row it would stand in for a day not yet synced.
pub async fn store_wellness(pool: &sqlx::SqlitePool, entries: Vec<WellnessEntry>) -> usize {
    use crate::data::db;

    let mut count = 0usize;
    for w in entries {
        let entry = db::WellnessEntry {
            date: w.date,
            hrv: w.hrv,
            resting_hr: w.resting_hr,
            sleep_secs: w.sleep_secs,
            sleep_score: w.sleep_score,
            steps: w.steps,
            calories: w.calories,
        };
        let empty = entry.hrv.is_none()
            && entry.resting_hr.is_none()
            && entry.sleep_secs.is_none()
            && entry.sleep_score.is_none()
            && entry.steps.is_none()
            && entry.calories.is_none();
        if empty {
            continue;
        }
        match db::upsert_wellness_entry(pool, &entry).await {
            Ok(()) => count += 1,
            Err(e) => tracing::error!("upsert wellness entry: {e}"),
        }
    }
    count
}

/// What a manual sync's toast adds when activities came back withheld:
/// nothing when none did.
pub fn withheld_suffix(withheld: usize) -> String {
    match withheld {
        0 => String::new(),
        1 => " — 1 more was withheld".to_string(),
        n => format!(" — {n} more were withheld"),
    }
}

/// The notice a routine sync raises once for each newly withheld activity,
/// with the one thing the rider can do about it.
pub fn withheld_notice(withheld: usize) -> String {
    match withheld {
        1 => {
            "Intervals.icu withheld 1 activity — upload its FIT file there to count it".to_string()
        }
        n => format!(
            "Intervals.icu withheld {n} activities — upload their FIT files there to count them"
        ),
    }
}

/// What a routine sync has to tell the rider.
#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    /// Activities in the sync window that Intervals.icu withheld.
    pub withheld: usize,
    /// Whether one of them is new since the rider was last told, so the
    /// notice is shown once per new withheld ride rather than every launch.
    pub announce: bool,
}

/// Whether `withheld` (newest first) holds an activity the rider has not yet
/// been told about — and if so, remember that they now have been.
///
/// Only the newest is remembered: rides arrive in date order, so a newer
/// withheld one is the only way the list can hold something new. One that ages
/// out of the sync window, or is uploaded and stops being withheld, raises
/// nothing.
async fn take_withheld_notice(pool: &sqlx::SqlitePool, withheld: &[String]) -> bool {
    use crate::data::settings;

    let Some(newest) = withheld.first() else {
        return false;
    };
    match settings::withheld_announced(pool).await {
        Ok(last) if last.as_deref() == Some(newest.as_str()) => false,
        Ok(_) => {
            if let Err(e) = settings::set_withheld_announced(pool, newest).await {
                tracing::error!("Could not record the withheld-activity notice: {e}");
            }
            true
        }
        Err(e) => {
            tracing::error!("Could not read the withheld-activity notice: {e}");
            false
        }
    }
}

/// Pull recent activities and wellness into the local database.
///
/// Errors are deliberately swallowed rather than propagated. This runs before
/// the morning brief, and a brief written against yesterday's data is worth far
/// more to a rider on a hotel wifi than no brief at all. What did arrive is
/// still saved; what did not is logged.
///
/// Does nothing without both credentials, which is not a failure — plenty of
/// riders never connect Intervals.icu.
pub async fn sync_recent(
    pool: &sqlx::SqlitePool,
    athlete_id: &str,
    api_key: &str,
    today: NaiveDate,
) -> SyncReport {
    if athlete_id.trim().is_empty() || api_key.trim().is_empty() {
        return SyncReport::default();
    }

    let mut report = SyncReport::default();
    match fetch_activities(
        athlete_id,
        api_key,
        today - chrono::Duration::days(SYNC_ACTIVITY_DAYS),
        today,
    )
    .await
    {
        Ok(activities) => {
            let stored = store_activities(pool, activities).await;
            report.withheld = stored.withheld.len();
            report.announce = take_withheld_notice(pool, &stored.withheld).await;
        }
        Err(e) => tracing::warn!("Intervals.icu activity sync failed: {e}"),
    }

    match fetch_wellness(
        athlete_id,
        api_key,
        today - chrono::Duration::days(SYNC_WELLNESS_DAYS),
        today,
    )
    .await
    {
        Ok(entries) => {
            store_wellness(pool, entries).await;
        }
        Err(e) => tracing::warn!("Intervals.icu wellness sync failed: {e}"),
    }

    report
}

/// How many activities one backfill pass downloads.
///
/// A five-hour ride's streams are about a megabyte. Ten a pass clears the
/// rider's summer of outdoor rides in a few launches without making any one
/// of them slow, and a first-time connection with years of history does not
/// start by pulling a gigabyte.
const STREAM_BACKFILL_BATCH: usize = 10;

/// How many uncached activities one pass may ask about, withheld ones included.
///
/// A withheld activity is left uncached so that it is fetched if Intervals.icu
/// starts sharing it again, which means it heads the queue every launch. Looking
/// this far past the batch keeps a run of them from starving the rides behind,
/// while bounding the requests a pass can spend on refusals.
const STREAM_BACKFILL_LOOKAHEAD: usize = STREAM_BACKFILL_BATCH * 3;

/// Download streams for synced rides and runs that have none cached, so the
/// Fitness page can count them in the power curve, the pace curve and the zone
/// bars. Newest first, [`STREAM_BACKFILL_BATCH`] at a time.
///
/// Runs after the routine sync, never ahead of it: nothing waits on it.
pub async fn backfill_streams(pool: &sqlx::SqlitePool, api_key: &str) {
    use crate::data::{db, settings};

    if api_key.trim().is_empty() {
        return;
    }

    match settings::streams_refetched(pool).await {
        Ok(true) => {}
        Ok(false) => {
            if let Err(e) = db::clear_activity_streams(pool).await {
                tracing::error!("Could not clear the old stream cache: {e}");
                return;
            }
            if let Err(e) = settings::set_streams_refetched(pool).await {
                tracing::error!("Could not record the stream cache clear-out: {e}");
            }
            tracing::info!("Cleared streams cached by the pre-0.12.0 fetch");
        }
        Err(e) => {
            tracing::error!("Could not read the stream cache flag: {e}");
            return;
        }
    }

    let api_key = api_key.to_string();
    backfill_streams_with(pool, |id| {
        let api_key = api_key.clone();
        async move { fetch_activity_streams(&api_key, &id).await }
    })
    .await;
}

/// The backfill pass itself, with the fetch passed in so it can be tested.
///
/// An activity Intervals.icu has no streams for is cached as `[]`, so it is not
/// asked for again every launch. A withheld one is skipped and the pass moves
/// on. A network error ends the pass without caching anything for that
/// activity — it is tried again next time. Returns how many were cached.
async fn backfill_streams_with<F, Fut>(pool: &sqlx::SqlitePool, fetch: F) -> usize
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<StreamFetch>>,
{
    use crate::data::db;

    let ids = match db::activities_missing_streams(pool, STREAM_BACKFILL_LOOKAHEAD).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("Could not list activities missing streams: {e}");
            return 0;
        }
    };

    let mut fetched = 0usize;
    let mut withheld = 0usize;
    for id in ids {
        if fetched == STREAM_BACKFILL_BATCH {
            break;
        }
        let json = match fetch(id.clone()).await {
            Ok(StreamFetch::Found(json)) => json,
            Ok(StreamFetch::Missing) => "[]".to_string(),
            Ok(StreamFetch::Withheld) => {
                withheld += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!("Stream backfill stopped: {e}");
                break;
            }
        };
        if let Err(e) = db::save_activity_streams(pool, &id, &json).await {
            tracing::error!("Could not cache streams for {id}: {e}");
            break;
        }
        fetched += 1;
    }
    if fetched > 0 {
        tracing::info!("Stream backfill cached {fetched} activities");
    }
    if withheld > 0 {
        tracing::warn!("Intervals.icu withheld the streams of {withheld} activities");
    }
    fetched
}

// ── Upload ────────────────────────────────────────────────────────────────────

/// Upload a completed session to Intervals.icu as a FIT file.
///
/// Sending the actual FIT binary (rather than a JSON summary) gives Intervals.icu full
/// time-series data — power curve, HR, cadence — which it can then sync to Garmin Connect
/// with complete data rather than a summary-only manual entry.
pub async fn upload_fit_activity(
    athlete_id: &str,
    api_key: &str,
    fit_bytes: Vec<u8>,
    name: &str,
) -> Result<()> {
    let client = make_client()?;
    let url = format!("{BASE_URL}/athlete/{athlete_id}/activities");

    let file_part = reqwest::multipart::Part::bytes(fit_bytes)
        .file_name("activity.fit")
        .mime_str("application/vnd.ant.fit")
        .context("invalid MIME type")?;
    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("name", name.to_string());

    let response = client
        .post(&url)
        .basic_auth("API_KEY", Some(api_key))
        .multipart(form)
        .send()
        .await
        .context("Intervals.icu FIT upload request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Intervals.icu upload error {status}: {body}");
    }

    tracing::debug!("Intervals.icu FIT activity uploaded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::db::{self, testing::test_pool};
    use std::cell::RefCell;

    /// Day `d` counted from the start of September, so a test can hold more
    /// rides than a month has days.
    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 1).expect("hardcoded valid date")
            + chrono::Duration::days(d.into())
    }

    /// A full ride, as Intervals.icu sends one it may share.
    fn ride(id: &str, d: u32) -> ActivitySummary {
        ActivitySummary {
            id: id.to_string(),
            name: "Morning Ride".to_string(),
            start_date_local: day(d),
            start_datetime_local: day(d).and_hms_opt(7, 0, 0),
            icu_training_load: Some(80.0),
            moving_time: Some(3600),
            average_watts: Some(180),
            normalized_watts: Some(195),
            average_hr: Some(140),
            max_hr: Some(171),
            sport_type: "Ride".to_string(),
            distance_m: Some(30_000.0),
            elevation_gain_m: Some(420.0),
            average_cadence: Some(88.0),
        }
    }

    /// The same ride as Intervals.icu sends one it may not share.
    fn stripped(id: &str, d: u32) -> ActivitySummary {
        ActivitySummary {
            id: id.to_string(),
            name: String::new(),
            start_date_local: day(d),
            start_datetime_local: None,
            icu_training_load: None,
            moving_time: None,
            average_watts: None,
            normalized_watts: None,
            average_hr: None,
            max_hr: None,
            sport_type: "Ride".to_string(),
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        }
    }

    fn wellness(d: u32, hrv: Option<f32>) -> WellnessEntry {
        WellnessEntry {
            date: day(d),
            hrv,
            resting_hr: None,
            sleep_secs: None,
            sleep_score: None,
            steps: None,
            calories: None,
        }
    }

    // ── Withheld activities ──────────────────────────────────────────────────

    #[test]
    fn should_call_a_ride_with_no_figure_at_all_withheld() {
        assert!(stripped("i1", 9).is_withheld());
    }

    #[test]
    fn should_not_call_a_ride_withheld_when_any_one_figure_survives() {
        // Each figure on its own is enough to make the copy a real ride. Zero
        // is a figure: a hand-entered ride can carry a load of 0.
        let only: [fn(&mut ActivitySummary); 9] = [
            |a| a.moving_time = Some(0),
            |a| a.icu_training_load = Some(0.0),
            |a| a.average_watts = Some(0),
            |a| a.normalized_watts = Some(0),
            |a| a.average_hr = Some(0),
            |a| a.max_hr = Some(0),
            |a| a.distance_m = Some(0.0),
            |a| a.elevation_gain_m = Some(0.0),
            |a| a.average_cadence = Some(0.0),
        ];
        for (i, set) in only.iter().enumerate() {
            let mut a = stripped("i1", 9);
            set(&mut a);
            assert!(!a.is_withheld(), "figure {i} alone should count");
        }
    }

    #[tokio::test]
    async fn should_not_let_a_stripped_copy_touch_the_ride_already_held() {
        let pool = test_pool().await;
        store_activities(&pool, vec![ride("i1", 9)]).await;
        let stored = store_activities(&pool, vec![stripped("i1", 9)]).await;

        assert_eq!(stored.saved, 0);
        assert_eq!(stored.withheld, vec!["i1".to_string()]);
        let held = db::load_intervals_activities(&pool).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].name, "Morning Ride");
        assert_eq!(held[0].tss, Some(80.0));
    }

    #[tokio::test]
    async fn should_not_store_a_ride_that_only_ever_arrived_stripped() {
        let pool = test_pool().await;
        let stored = store_activities(&pool, vec![stripped("i2", 10), ride("i1", 9)]).await;

        assert_eq!(stored.saved, 1);
        let held = db::load_intervals_activities(&pool).await.unwrap();
        assert_eq!(
            held.iter().map(|a| a.icu_id.as_str()).collect::<Vec<_>>(),
            ["i1"]
        );
    }

    #[tokio::test]
    async fn should_list_withheld_rides_newest_first() {
        let pool = test_pool().await;
        let stored = store_activities(
            &pool,
            vec![stripped("i5", 5), stripped("i9", 9), stripped("i7", 7)],
        )
        .await;
        assert_eq!(stored.withheld, ["i9", "i7", "i5"]);
    }

    #[tokio::test]
    async fn should_announce_a_withheld_ride_once() {
        let pool = test_pool().await;
        let ids = vec!["i9".to_string(), "i7".to_string()];
        assert!(take_withheld_notice(&pool, &ids).await);
        assert!(!take_withheld_notice(&pool, &ids).await);
    }

    #[tokio::test]
    async fn should_announce_again_when_a_newer_ride_is_withheld() {
        let pool = test_pool().await;
        take_withheld_notice(&pool, &["i7".to_string()]).await;
        assert!(take_withheld_notice(&pool, &["i9".to_string(), "i7".to_string()]).await);
    }

    #[tokio::test]
    async fn should_not_announce_when_nothing_is_withheld() {
        let pool = test_pool().await;
        assert!(!take_withheld_notice(&pool, &[]).await);
        // And nothing was recorded, so the first real one still announces.
        assert!(take_withheld_notice(&pool, &["i1".to_string()]).await);
    }

    #[test]
    fn should_say_one_activity_and_not_one_activities() {
        assert_eq!(withheld_suffix(0), "");
        assert_eq!(withheld_suffix(1), " — 1 more was withheld");
        assert_eq!(withheld_suffix(2), " — 2 more were withheld");
        assert!(withheld_notice(1).contains("withheld 1 activity —"));
        assert!(withheld_notice(1).contains("its FIT file"));
        assert!(withheld_notice(3).contains("withheld 3 activities —"));
        assert!(withheld_notice(3).contains("their FIT files"));
    }

    // ── Wellness ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_not_store_a_day_with_no_reading() {
        let pool = test_pool().await;
        let count = store_wellness(&pool, vec![wellness(9, None), wellness(8, Some(60.0))]).await;
        assert_eq!(count, 1);
        let held = db::load_wellness_recent(&pool, 30).await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].date, day(8));
    }

    // ── Streams ──────────────────────────────────────────────────────────────

    #[test]
    fn should_read_only_404_and_403_as_answers_about_the_activity() {
        assert_eq!(stream_refusal(404), Some(StreamFetch::Missing));
        assert_eq!(stream_refusal(403), Some(StreamFetch::Withheld));
        // A bad key, a rate limit, a fault: try again next launch.
        for status in [400, 401, 429, 500, 502, 503] {
            assert_eq!(stream_refusal(status), None, "status {status}");
        }
    }

    /// Activities `i1`..`i{n}`, `i{n}` newest, none with streams.
    async fn pool_with_rides(n: u32) -> sqlx::SqlitePool {
        let pool = test_pool().await;
        store_activities(&pool, (1..=n).map(|d| ride(&format!("i{d}"), d)).collect()).await;
        pool
    }

    #[tokio::test]
    async fn should_step_past_withheld_streams_to_the_rides_behind() {
        // The newest dozen are withheld — more than a whole batch. The pass
        // must reach the two behind them rather than spend itself on refusals.
        let pool = pool_with_rides(14).await;
        let fetched = backfill_streams_with(&pool, |id| async move {
            let n: u32 = id[1..].parse().unwrap();
            Ok(if n > 2 {
                StreamFetch::Withheld
            } else {
                StreamFetch::Found(format!("[{n}]"))
            })
        })
        .await;

        assert_eq!(fetched, 2);
        assert_eq!(
            db::get_activity_streams(&pool, "i1")
                .await
                .unwrap()
                .as_deref(),
            Some("[1]")
        );
        assert_eq!(
            db::get_activity_streams(&pool, "i2")
                .await
                .unwrap()
                .as_deref(),
            Some("[2]")
        );
    }

    #[tokio::test]
    async fn should_leave_withheld_streams_uncached_so_they_are_asked_for_again() {
        let pool = pool_with_rides(1).await;
        backfill_streams_with(&pool, |_| async { Ok(StreamFetch::Withheld) }).await;
        assert_eq!(db::get_activity_streams(&pool, "i1").await.unwrap(), None);
        assert_eq!(
            db::activities_missing_streams(&pool, 10).await.unwrap(),
            ["i1"]
        );
    }

    #[tokio::test]
    async fn should_cache_a_ride_with_no_streams_as_empty() {
        let pool = pool_with_rides(1).await;
        backfill_streams_with(&pool, |_| async { Ok(StreamFetch::Missing) }).await;
        assert_eq!(
            db::get_activity_streams(&pool, "i1")
                .await
                .unwrap()
                .as_deref(),
            Some("[]")
        );
    }

    #[tokio::test]
    async fn should_stop_the_pass_at_an_error_and_cache_nothing_for_it() {
        let pool = pool_with_rides(3).await;
        let asked = RefCell::new(Vec::new());
        backfill_streams_with(&pool, |id| {
            asked.borrow_mut().push(id.clone());
            async move {
                if id == "i3" {
                    Ok(StreamFetch::Found("[3]".to_string()))
                } else {
                    Err(anyhow::anyhow!("rate limited"))
                }
            }
        })
        .await;

        assert_eq!(*asked.borrow(), ["i3", "i2"]);
        assert_eq!(db::get_activity_streams(&pool, "i2").await.unwrap(), None);
    }

    #[tokio::test]
    async fn should_fetch_no_more_than_one_batch_a_pass() {
        let pool = pool_with_rides(STREAM_BACKFILL_BATCH as u32 + 3).await;
        let fetched =
            backfill_streams_with(&pool, |_| async { Ok(StreamFetch::Found("[]".into())) }).await;
        assert_eq!(fetched, STREAM_BACKFILL_BATCH);
    }

    #[tokio::test]
    async fn should_give_up_on_refusals_after_the_lookahead() {
        let pool = pool_with_rides(STREAM_BACKFILL_LOOKAHEAD as u32 + 5).await;
        let asked = RefCell::new(0usize);
        backfill_streams_with(&pool, |_| {
            *asked.borrow_mut() += 1;
            async { Ok(StreamFetch::Withheld) }
        })
        .await;
        assert_eq!(*asked.borrow(), STREAM_BACKFILL_LOOKAHEAD);
    }

    #[test]
    fn should_ask_for_streams_at_the_documented_path() {
        assert_eq!(
            streams_url("i178814881"),
            "https://intervals.icu/api/v1/activity/i178814881/streams\
             ?types=time,watts,heartrate,cadence,distance,altitude,latlng,velocity_smooth"
        );
    }
}
