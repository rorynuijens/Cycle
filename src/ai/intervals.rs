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

/// Fetch per-second time-series streams for a single activity.
///
/// Returns the raw JSON (array-of-objects format) so the caller can cache it
/// verbatim, or `None` when Intervals.icu has no streams for the activity — a
/// manually entered ride, say. Anything else is an error, so a network failure
/// is never mistaken for a ride without data.
async fn fetch_activity_streams(api_key: &str, activity_id: &str) -> Result<Option<String>> {
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
        if status.as_u16() == 404 {
            tracing::debug!(body = %body, "Streams endpoint 404");
            return Ok(None);
        }
        anyhow::bail!("Intervals.icu streams API error {status}: {body}");
    }

    response
        .text()
        .await
        .map(Some)
        .context("failed to read streams response")
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
    let streams = fetch_activity_streams(api_key, activity_id).await?;
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
) {
    use crate::data::db;

    if athlete_id.trim().is_empty() || api_key.trim().is_empty() {
        return;
    }

    match fetch_activities(
        athlete_id,
        api_key,
        today - chrono::Duration::days(SYNC_ACTIVITY_DAYS),
        today,
    )
    .await
    {
        Ok(activities) => {
            for a in activities {
                let _ = db::upsert_intervals_activity(
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
                .await;
            }
            // A ride recorded in-app can arrive back here after a round trip
            // through Garmin or Strava — link the two so it is shown and
            // counted once.
            if let Err(e) = db::reconcile_icu_links(pool).await {
                tracing::error!("reconcile_icu_links: {e}");
            }
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
                let _ = db::upsert_wellness_entry(pool, &entry).await;
            }
        }
        Err(e) => tracing::warn!("Intervals.icu wellness sync failed: {e}"),
    }
}

/// How many activities one backfill pass downloads.
///
/// A five-hour ride's streams are about a megabyte. Ten a pass clears the
/// rider's summer of outdoor rides in a few launches without making any one
/// of them slow, and a first-time connection with years of history does not
/// start by pulling a gigabyte.
const STREAM_BACKFILL_BATCH: usize = 10;

/// Download streams for synced rides and runs that have none cached, so the
/// Fitness page can count them in the power curve, the pace curve and the zone
/// bars. Newest first, [`STREAM_BACKFILL_BATCH`] at a time.
///
/// Runs after the routine sync, never ahead of it: nothing waits on it. An
/// activity Intervals.icu has no streams for is cached as `[]`, so it is not
/// asked for again every launch. A network error ends the pass without caching
/// anything for that activity — it is tried again next time.
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

    let ids = match db::activities_missing_streams(pool, STREAM_BACKFILL_BATCH).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("Could not list activities missing streams: {e}");
            return;
        }
    };

    let mut fetched = 0usize;
    for id in ids {
        let json = match fetch_activity_streams(api_key, &id).await {
            Ok(Some(json)) => json,
            Ok(None) => "[]".to_string(),
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

    #[test]
    fn should_ask_for_streams_at_the_documented_path() {
        assert_eq!(
            streams_url("i178814881"),
            "https://intervals.icu/api/v1/activity/i178814881/streams\
             ?types=time,watts,heartrate,cadence,distance,altitude,latlng,velocity_smooth"
        );
    }
}
