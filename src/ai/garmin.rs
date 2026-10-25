//! Garmin Connect Personal API — read-only.
//!
//! The source of the rider's activities, activity files and wellness once a
//! personal key is set; [`sync`] stores them. Results are mapped into the
//! [`intervals`](super::intervals) shapes, because Garmin data lands in the
//! same tables the Intervals.icu sync fills, under ids prefixed `g`.
//!
//! The personal key is never sent to the data endpoints. It is exchanged for an
//! access token that lasts 24 hours, with no refresh token — a new one is simply
//! requested ([personal API docs]). The token lives in memory only.
//!
//! [personal API docs]: https://developer.garmin.com/gc-developer-program/personal/personal-api/

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;

use super::intervals::{ActivitySummary, WellnessEntry};

const BASE_URL: &str = "https://personal.connectapi.garmin.com";
const TOKEN_PATH: &str = "/personalapitoken-service/personal-api/personal-api-key/token";

/// The most activities the search endpoint returns per page.
const PAGE_LIMIT: usize = 99;
/// Sleep and steps are capped at 31 and 28 days per request; one chunk size
/// that satisfies every range endpoint keeps the client simple.
const RANGE_CHUNK_DAYS: i64 = 28;
/// No legitimate activity file comes near this, zipped or not (CLAUDE.md §5.3).
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

fn make_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// A request Garmin answered with an error status.
///
/// Kept typed so a caller can tell a key Garmin no longer accepts (401) or a
/// file it no longer holds (404) from a passing failure; see [`status_of`].
#[derive(Debug, thiserror::Error)]
#[error("Garmin API error {status} on {what}: {body}")]
pub struct StatusError {
    pub status: u16,
    what: String,
    body: String,
}

/// The HTTP status Garmin refused a request with, anywhere in `err`'s chain.
pub fn status_of(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|c| c.downcast_ref::<StatusError>())
        .map(|e| e.status)
}

fn refused(status: reqwest::StatusCode, what: &str, body: String) -> anyhow::Error {
    StatusError {
        status: status.as_u16(),
        what: what.to_string(),
        body,
    }
    .into()
}

// ── Token ─────────────────────────────────────────────────────────────────────

/// A Garmin access token. `Debug` is deliberately not derived so it cannot be
/// logged by accident (CLAUDE.md §5.4).
pub struct AccessToken(String);

/// Pull the access token out of the token endpoint's answer.
///
/// Garmin's docs do not show the response body, so the usual OAuth2 spellings
/// are all accepted. A failure names only the fields that were present — never
/// their values, any of which may be a credential.
fn parse_token(body: &str) -> Result<AccessToken> {
    let body = body.trim();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        // Not JSON: accept a bare token, which is one run of printable,
        // non-space characters. Anything else (an HTML page, say) is described
        // by its shape only, since it might still hold the credential.
        let bare = !body.is_empty()
            && !body.starts_with('<')
            && body.chars().all(|c| c.is_ascii_graphic());
        if bare {
            return Ok(AccessToken(body.to_string()));
        }
        anyhow::bail!(
            "token response is neither JSON nor a bare token ({} chars, starts with {:?})",
            body.chars().count(),
            body.chars().next().filter(|c| !c.is_alphanumeric())
        );
    };
    // A JSON string is a bare token that was quoted.
    if let Some(token) = value.as_str().filter(|t| !t.trim().is_empty()) {
        return Ok(AccessToken(token.trim().to_string()));
    }
    for field in ["access_token", "accessToken", "token"] {
        if let Some(token) = value.get(field).and_then(|v| v.as_str()) {
            if !token.trim().is_empty() {
                return Ok(AccessToken(token.trim().to_string()));
            }
        }
    }
    let fields: Vec<&str> = value
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    anyhow::bail!("token response has no access token (fields: {fields:?})")
}

/// Exchange the personal API key for a 24-hour access token.
pub async fn fetch_token(api_key: &str) -> Result<AccessToken> {
    let response = make_client()?
        .post(format!("{BASE_URL}{TOKEN_PATH}"))
        .header("Authorization", format!("ApiKey {}", api_key.trim()))
        .send()
        .await
        .context("Garmin token request failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The body of a refused token request is an error message, not a token.
        return Err(refused(status, "the token request", body));
    }
    tracing::debug!("Garmin access token acquired (not logged)");
    parse_token(&body)
}

/// How many times one request waits out a rate limit before giving up.
const RATE_LIMIT_RETRIES: u32 = 5;

/// How long Garmin asks a refused client to wait.
///
/// Garmin allows 100 requests per window and answers the 101st with a 429
/// whose body says when the window ends (`{"count":101,"limit":100,
/// "retryInSeconds":17}`). Refused requests still count, so the wait is
/// honoured rather than retried at once. A body without the figure waits a
/// whole minute, the longest window seen.
fn retry_after(body: &str) -> std::time::Duration {
    let secs = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("retryInSeconds")?.as_u64())
        .unwrap_or(60)
        .min(300);
    // One second over, so the request lands after the window, not on its edge.
    std::time::Duration::from_secs(secs + 1)
}

/// One GET against the data API, waiting out rate limits.
async fn send_get(token: &AccessToken, url: &str) -> Result<reqwest::Response> {
    let client = make_client()?;
    let mut attempt = 0;
    loop {
        let response = client
            .get(url)
            .bearer_auth(&token.0)
            .send()
            .await
            .with_context(|| format!("Garmin request failed: {url}"))?;
        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS
            || attempt == RATE_LIMIT_RETRIES
        {
            return Ok(response);
        }
        attempt += 1;
        let wait = retry_after(&response.text().await.unwrap_or_default());
        tracing::info!("Garmin rate limit reached; waiting {} s", wait.as_secs());
        tokio::time::sleep(wait).await;
    }
}

/// One GET against the data API. `Ok(None)` is a 204 — Garmin's "no data".
async fn get_text(token: &AccessToken, path_and_query: &str) -> Result<Option<String>> {
    let response = send_get(token, &format!("{BASE_URL}{path_and_query}")).await?;
    let status = response.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(refused(status, path_and_query, body));
    }
    Ok(Some(body))
}

// ── Activities ────────────────────────────────────────────────────────────────

/// One activity as the search endpoint describes it, with the fields that have
/// no Intervals.icu counterpart kept alongside the shared ones.
#[derive(Debug, Clone)]
pub struct GarminActivity {
    pub summary: ActivitySummary,
    /// Garmin's own TSS — only present when the device knew an FTP. The sync
    /// works out its own load instead (see [`sync`]); only the comparison
    /// harness reads this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub training_stress_score: Option<f32>,
    /// Whether the ride has a GPS track, i.e. was ridden outdoors. Read by the
    /// comparison harness; the sync takes the track from the file instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub has_gps: bool,
    /// The FTP the head unit scored the ride against: normalised power over
    /// the intensity factor, both from the activity list. Garmin keeps no FTP
    /// field of its own that is filled in reliably (`maxFtp` mostly is not).
    /// Taken as it comes — see [`sync`] for the 200 W Garmin assumes unset.
    pub set_ftp: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawActivityType {
    #[serde(default)]
    type_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawActivity {
    activity_id: i64,
    #[serde(default)]
    activity_name: Option<String>,
    #[serde(default)]
    start_time_local: Option<String>,
    #[serde(default)]
    activity_type: Option<RawActivityType>,
    #[serde(default)]
    distance: Option<f64>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    moving_duration: Option<f64>,
    #[serde(default)]
    elevation_gain: Option<f64>,
    #[serde(default, rename = "averageHR")]
    average_hr: Option<f64>,
    #[serde(default, rename = "maxHR")]
    max_hr: Option<f64>,
    #[serde(default)]
    average_biking_cadence_in_rev_per_minute: Option<f64>,
    #[serde(default)]
    avg_power: Option<f64>,
    #[serde(default)]
    norm_power: Option<f64>,
    #[serde(default)]
    training_stress_score: Option<f64>,
    #[serde(default)]
    intensity_factor: Option<f64>,
    #[serde(default)]
    has_polyline: Option<bool>,
    #[serde(default)]
    start_latitude: Option<f64>,
}

/// Garmin writes local start times as `2026-10-20 08:30:00`; the ISO `T`
/// spelling and fractional seconds are accepted too.
fn parse_local_time(s: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(s, fmt).ok())
}

/// A count or reading from Garmin, rounded, with negatives and non-numbers
/// discarded rather than wrapped.
fn whole(v: Option<f64>) -> Option<u32> {
    v.filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.round().min(u32::MAX as f64) as u32)
}

fn real(v: Option<f64>) -> Option<f32> {
    v.filter(|v| v.is_finite()).map(|v| v as f32)
}

/// Parse one page of the activity search. An activity without a readable
/// start time is dropped: it could not be matched against anything.
fn parse_activities(body: &str) -> Result<Vec<GarminActivity>> {
    let raw: Vec<RawActivity> =
        serde_json::from_str(body).context("failed to parse Garmin activities")?;
    Ok(raw
        .into_iter()
        .filter_map(|r| {
            let start = parse_local_time(r.start_time_local.as_deref()?)?;
            let has_gps = r.has_polyline.unwrap_or(false) || r.start_latitude.is_some();
            let set_ftp = match (r.norm_power, r.intensity_factor) {
                (Some(np), Some(f)) if f > 0.0 => whole(Some(np / f)).filter(|w| *w > 0),
                _ => None,
            };
            Some(GarminActivity {
                summary: ActivitySummary {
                    id: r.activity_id.to_string(),
                    name: r.activity_name.unwrap_or_default(),
                    start_date_local: start.date(),
                    start_datetime_local: Some(start),
                    // Garmin has no load of its own beyond TSS, kept separately.
                    icu_training_load: None,
                    // Zero moving time is Garmin finding no speed in the
                    // file (the app's own rides), not a ride that never moved.
                    moving_time: whole(r.moving_duration)
                        .filter(|s| *s > 0)
                        .or_else(|| whole(r.duration).filter(|s| *s > 0)),
                    average_watts: whole(r.avg_power),
                    normalized_watts: whole(r.norm_power),
                    average_hr: whole(r.average_hr),
                    max_hr: whole(r.max_hr),
                    sport_type: r
                        .activity_type
                        .unwrap_or_default()
                        .type_key
                        .unwrap_or_default(),
                    distance_m: real(r.distance),
                    elevation_gain_m: real(r.elevation_gain),
                    average_cadence: real(r.average_biking_cadence_in_rev_per_minute),
                },
                training_stress_score: real(r.training_stress_score),
                has_gps,
                set_ftp,
            })
        })
        .collect())
}

/// Garmin's activity type key, spelled the way Intervals.icu spells it.
///
/// The app stores sports exactly as they arrive and classifies them in
/// `data/sport.rs` and `data/dedupe.rs`, which know the Intervals.icu names.
/// A Garmin key with no counterpart passes through unchanged, so it is shown
/// under its own name rather than counted as something it is not.
pub fn icu_sport_type(garmin_key: &str) -> String {
    match garmin_key {
        "cycling" | "road_biking" | "cyclocross" | "track_cycling" | "recumbent_cycling" => "Ride",
        "indoor_cycling" | "virtual_ride" => "VirtualRide",
        "gravel_cycling" => "GravelRide",
        "mountain_biking" | "downhill_biking" | "bmx" => "MountainBikeRide",
        "e_bike_fitness" | "e_bike_mountain" => "EBikeRide",
        "hand_cycling" | "indoor_hand_cycling" => "Handcycle",
        "running" | "street_running" | "track_running" | "obstacle_run" => "Run",
        "trail_running" => "TrailRun",
        "treadmill_running" | "indoor_running" => "TreadmillRun",
        "virtual_run" => "VirtualRun",
        "walking" | "casual_walking" | "speed_walking" => "Walk",
        "hiking" => "Hike",
        "strength_training" => "WeightTraining",
        "lap_swimming" => "Swim",
        "open_water_swimming" => "OpenWaterSwim",
        "yoga" => "Yoga",
        "indoor_rowing" | "rowing" => "Rowing",
        "elliptical" => "Elliptical",
        "hiit" => "HighIntensityIntervalTraining",
        // An empty sport means "the app's own ride" to data/sport.rs, which a
        // Garmin activity with no type is not.
        "" => "Other",
        other => other,
    }
    .to_string()
}

/// Every activity (any sport) that started between two dates, inclusive.
pub async fn fetch_activities(
    token: &AccessToken,
    oldest: NaiveDate,
    newest: NaiveDate,
) -> Result<Vec<GarminActivity>> {
    let mut all = Vec::new();
    for page in 0.. {
        let start = page * PAGE_LIMIT;
        let path = format!(
            "/activitylist-service/activities/search/activities\
             ?startDate={oldest}&endDate={newest}&start={start}&limit={PAGE_LIMIT}"
        );
        let body = get_text(token, &path).await?.unwrap_or_else(|| "[]".into());
        let raw_count = serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .map(|v| v.len())
            .unwrap_or(0);
        all.extend(parse_activities(&body)?);
        // A short page is the last one. Counting the raw page, not what parsed,
        // keeps one unreadable activity from ending the walk early.
        if raw_count < PAGE_LIMIT {
            break;
        }
    }
    Ok(all)
}

/// The day of the rider's first activity of any kind, from an oldest-first
/// search; `None` for an account with no activities.
pub async fn fetch_earliest_activity(token: &AccessToken) -> Result<Option<NaiveDate>> {
    let body = get_text(
        token,
        "/activitylist-service/activities/search/activities?sortOrder=asc&limit=1",
    )
    .await?
    .unwrap_or_else(|| "[]".into());
    Ok(parse_activities(&body)?
        .first()
        .map(|a| a.summary.start_date_local))
}

/// Pull the FIT file out of what the download endpoint sent.
///
/// The endpoint is documented as returning a zip of the original upload, but a
/// bare FIT is accepted too. Every size is capped before it is trusted, so a
/// zip that claims a small entry and inflates without end is cut off.
fn extract_fit(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() >= 12 && &bytes[8..12] == b".FIT" {
        return Ok(bytes.to_vec());
    }
    anyhow::ensure!(
        bytes.starts_with(b"PK"),
        "download is neither a zip nor a FIT file"
    );
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("unreadable activity zip")?;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).context("unreadable zip entry")?;
        let is_fit = entry
            .name()
            .is_ok_and(|n| n.to_ascii_lowercase().ends_with(".fit"));
        if !is_fit {
            continue;
        }
        let mut out = Vec::new();
        entry
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut out)
            .context("failed to inflate FIT entry")?;
        anyhow::ensure!(
            out.len() as u64 <= MAX_FILE_BYTES,
            "FIT file inside zip is too large"
        );
        return Ok(out);
    }
    anyhow::bail!("activity zip holds no .fit file")
}

/// Download one activity's FIT file.
pub async fn fetch_activity_fit(token: &AccessToken, activity_id: &str) -> Result<Vec<u8>> {
    let url = format!("{BASE_URL}/download-service/files/activity/{activity_id}");
    let response = send_get(token, &url)
        .await
        .context("Garmin activity download failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(refused(status, "an activity download", body));
    }
    if let Some(len) = response.content_length() {
        anyhow::ensure!(
            len <= MAX_FILE_BYTES,
            "activity download too large ({len} bytes)"
        );
    }
    let bytes = response
        .bytes()
        .await
        .context("failed to read activity download")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "activity download too large"
    );
    extract_fit(&bytes)
}

// ── Wellness ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHrvDaily {
    #[serde(default)]
    hrv_summaries: Vec<RawHrv>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHrv {
    #[serde(default)]
    calendar_date: Option<String>,
    #[serde(default)]
    last_night_avg: Option<f64>,
}

#[derive(Deserialize)]
struct RawScore {
    #[serde(default)]
    value: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSleep {
    #[serde(default)]
    calendar_date: Option<String>,
    #[serde(default)]
    sleep_time_seconds: Option<f64>,
    #[serde(default)]
    sleep_scores: BTreeMap<String, RawScore>,
}

#[derive(Deserialize)]
struct RawSummaries {
    #[serde(default)]
    data: Vec<RawSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSummary {
    #[serde(default)]
    calendar_date: Option<String>,
    #[serde(default)]
    heart_rate: Option<RawHeartRate>,
    #[serde(default)]
    movement: Option<RawMovement>,
    #[serde(default)]
    calories: Option<RawCalories>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHeartRate {
    #[serde(default)]
    resting_value: Option<f64>,
}

#[derive(Deserialize)]
struct RawMovement {
    #[serde(default)]
    steps: Option<RawScore>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCalories {
    #[serde(default)]
    burned_total: Option<f64>,
}

fn date_of(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?.get(..10)?, "%Y-%m-%d").ok()
}

/// The day's entry in `days`, created empty on first sight.
fn day(days: &mut BTreeMap<NaiveDate, WellnessEntry>, date: NaiveDate) -> &mut WellnessEntry {
    days.entry(date).or_insert_with(|| WellnessEntry {
        date,
        hrv: None,
        resting_hr: None,
        sleep_secs: None,
        sleep_score: None,
        steps: None,
        calories: None,
    })
}

fn merge_hrv(days: &mut BTreeMap<NaiveDate, WellnessEntry>, body: &str) -> Result<()> {
    let raw: RawHrvDaily = serde_json::from_str(body).context("failed to parse Garmin HRV")?;
    for h in raw.hrv_summaries {
        if let Some(date) = date_of(h.calendar_date.as_deref()) {
            day(days, date).hrv = real(h.last_night_avg).filter(|v| *v > 0.0);
        }
    }
    Ok(())
}

fn merge_sleep(days: &mut BTreeMap<NaiveDate, WellnessEntry>, body: &str) -> Result<()> {
    let raw: Vec<RawSleep> = serde_json::from_str(body).context("failed to parse Garmin sleep")?;
    for s in raw {
        if let Some(date) = date_of(s.calendar_date.as_deref()) {
            let entry = day(days, date);
            entry.sleep_secs = whole(s.sleep_time_seconds).filter(|v| *v > 0);
            entry.sleep_score = s.sleep_scores.get("overall").and_then(|o| whole(o.value));
        }
    }
    Ok(())
}

fn merge_summaries(days: &mut BTreeMap<NaiveDate, WellnessEntry>, body: &str) -> Result<()> {
    let raw: RawSummaries =
        serde_json::from_str(body).context("failed to parse Garmin daily summaries")?;
    for s in raw.data {
        if let Some(date) = date_of(s.calendar_date.as_deref()) {
            let entry = day(days, date);
            entry.resting_hr = s.heart_rate.and_then(|h| whole(h.resting_value));
            entry.steps = s
                .movement
                .and_then(|m| m.steps)
                .and_then(|v| whole(v.value));
            entry.calories = s.calories.and_then(|c| whole(c.burned_total));
        }
    }
    Ok(())
}

/// `[oldest, newest]` cut into inclusive chunks no endpoint will refuse.
fn chunks(oldest: NaiveDate, newest: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    let mut from = oldest;
    while from <= newest {
        let to = (from + Duration::days(RANGE_CHUNK_DAYS - 1)).min(newest);
        out.push((from, to));
        from = to + Duration::days(1);
    }
    out
}

/// HRV, sleep and daily summaries between two dates, merged into one entry
/// per day in the shape Intervals.icu wellness uses.
pub async fn fetch_wellness(
    token: &AccessToken,
    oldest: NaiveDate,
    newest: NaiveDate,
) -> Result<Vec<WellnessEntry>> {
    let mut days = BTreeMap::new();
    for (from, to) in chunks(oldest, newest) {
        if let Some(body) = get_text(token, &format!("/hrv-service/hrv/daily/{from}/{to}")).await? {
            merge_hrv(&mut days, &body)?;
        }
        let sleep =
            format!("/sleep-service/sleep/dailySleepSummariesByDate?startDate={from}&endDate={to}");
        if let Some(body) = get_text(token, &sleep).await? {
            merge_sleep(&mut days, &body)?;
        }
        let summaries =
            format!("/usersummary-service/v2/dailysummaries?startDate={from}&endDate={to}");
        if let Some(body) = get_text(token, &summaries).await? {
            merge_summaries(&mut days, &body)?;
        }
    }
    Ok(days.into_values().collect())
}

pub mod sync;

#[cfg(test)]
mod compare;

#[cfg(test)]
mod import;

#[cfg(test)]
mod load;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_each_oauth_spelling_of_the_token() {
        for body in [
            r#"{"access_token":"abc","expires_in":86400}"#,
            r#"{"accessToken":"abc"}"#,
            r#"{"token":" abc "}"#,
        ] {
            assert_eq!(parse_token(body).unwrap().0, "abc", "{body}");
        }
    }

    #[test]
    fn should_name_fields_but_never_values_when_the_token_is_missing() {
        let err = parse_token(r#"{"refresh":"s3cret","access_token":""}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("refresh"), "{err}");
        assert!(
            !err.contains("s3cret"),
            "a credential leaked into an error: {err}"
        );
    }

    #[test]
    fn should_reject_a_token_response_that_is_an_html_page() {
        let err = parse_token("<html>gateway timeout</html>")
            .err()
            .unwrap()
            .to_string();
        assert!(
            !err.contains("gateway"),
            "the body leaked into an error: {err}"
        );
        assert!(parse_token("").is_err());
        assert!(parse_token("two words").is_err());
    }

    #[test]
    fn should_accept_a_bare_or_quoted_token() {
        assert_eq!(
            parse_token("eyJhbGciOi.payload.sig\n").unwrap().0,
            "eyJhbGciOi.payload.sig"
        );
        assert_eq!(parse_token("\"eyJ.abc.def\"").unwrap().0, "eyJ.abc.def");
    }

    #[test]
    fn should_map_a_ride_from_the_search_endpoint() {
        let body = r#"[{"activityId":21234567890,"activityName":"Morning Ride",
            "startTimeLocal":"2026-10-20 08:30:00","activityType":{"typeKey":"road_biking"},
            "distance":42195.5,"duration":5400.9,"movingDuration":5100.4,"elevationGain":312.0,
            "averageHR":141.6,"maxHR":172.0,"averageBikingCadenceInRevPerMinute":86.0,
            "avgPower":187.4,"normPower":203.5,"trainingStressScore":88.1,
            "hasPolyline":true}]"#;
        let a = &parse_activities(body).unwrap()[0];
        assert_eq!(a.summary.id, "21234567890");
        assert_eq!(
            a.summary.start_datetime_local,
            NaiveDate::from_ymd_opt(2026, 10, 20).and_then(|d| d.and_hms_opt(8, 30, 0))
        );
        // Moving time wins over elapsed; both round to the nearest second.
        assert_eq!(a.summary.moving_time, Some(5100));
        assert_eq!(a.summary.average_watts, Some(187));
        assert_eq!(a.summary.normalized_watts, Some(204));
        assert_eq!(a.summary.average_hr, Some(142));
        assert_eq!(a.summary.sport_type, "road_biking");
        assert_eq!(a.training_stress_score, Some(88.1));
        assert!(a.has_gps);
        // No intensity factor in the listing: no set FTP to read off it.
        assert_eq!(a.set_ftp, None);
    }

    #[test]
    fn should_read_the_set_ftp_off_normalised_power_and_intensity_factor() {
        let body = r#"[{"activityId":1,"startTimeLocal":"2026-10-20 08:30:00",
            "normPower":203.5,"intensityFactor":0.85},
            {"activityId":2,"startTimeLocal":"2026-10-20 08:30:00",
            "normPower":203.5,"intensityFactor":0},
            {"activityId":3,"startTimeLocal":"2026-10-20 08:30:00",
            "normPower":203.5,"intensityFactor":-0.5},
            {"activityId":4,"startTimeLocal":"2026-10-20 08:30:00","intensityFactor":0.85}]"#;
        let ftps: Vec<_> = parse_activities(body)
            .unwrap()
            .into_iter()
            .map(|a| a.set_ftp)
            .collect();
        // 203.5 / 0.85 = 239.4. A zero or negative factor, or no power, says nothing.
        assert_eq!(ftps, [Some(239), None, None, None]);
    }

    #[test]
    fn should_find_the_status_of_a_refusal_through_added_context() {
        let err = refused(reqwest::StatusCode::UNAUTHORIZED, "x", String::new())
            .context("Garmin activity download failed");
        assert_eq!(status_of(&err), Some(401));
        assert_eq!(status_of(&anyhow::anyhow!("timed out")), None);
    }

    #[test]
    fn should_fall_back_to_elapsed_time_when_garmin_says_zero_moving_time() {
        // What Garmin says of a ride the app recorded and the rider uploaded:
        // no speed in the file, so nothing it counts as moving.
        let body = r#"[{"activityId":1,"startTimeLocal":"2026-05-18 16:02:30",
            "duration":697.4,"movingDuration":0.0}]"#;
        assert_eq!(
            parse_activities(body).unwrap()[0].summary.moving_time,
            Some(697)
        );
        let neither = r#"[{"activityId":1,"startTimeLocal":"2026-05-18 16:02:30",
            "duration":0,"movingDuration":0}]"#;
        assert_eq!(
            parse_activities(neither).unwrap()[0].summary.moving_time,
            None
        );
    }

    #[test]
    fn should_survive_an_activity_with_every_optional_field_null() {
        let body = r#"[{"activityId":1,"startTimeLocal":"2026-10-20T08:30:00.0",
            "activityName":null,"activityType":null,"avgPower":null,"averageHR":null}]"#;
        let a = &parse_activities(body).unwrap()[0];
        assert_eq!(a.summary.name, "");
        assert_eq!(a.summary.sport_type, "");
        assert_eq!(a.summary.average_watts, None);
        assert!(!a.has_gps);
    }

    #[test]
    fn should_drop_an_activity_whose_start_cannot_be_read_and_keep_the_rest() {
        let body = r#"[{"activityId":1,"startTimeLocal":"yesterday"},
            {"activityId":2},{"activityId":3,"startTimeLocal":"2026-10-20 08:30:00"}]"#;
        let ids: Vec<_> = parse_activities(body)
            .unwrap()
            .into_iter()
            .map(|a| a.summary.id)
            .collect();
        assert_eq!(ids, ["3"]);
    }

    #[test]
    fn should_spell_garmin_sports_so_the_app_classifies_them_as_icu_ones() {
        use crate::data::sport::{is_cycling, is_indoor, is_run};
        for k in [
            "road_biking",
            "gravel_cycling",
            "mountain_biking",
            "cycling",
        ] {
            assert!(is_cycling(&icu_sport_type(k)), "{k}");
            assert!(!is_indoor(&icu_sport_type(k)), "{k}");
        }
        for k in ["indoor_cycling", "virtual_ride"] {
            assert!(is_indoor(&icu_sport_type(k)), "{k}");
        }
        for k in ["running", "trail_running", "treadmill_running"] {
            assert!(is_run(&icu_sport_type(k)), "{k}");
        }
        assert_eq!(icu_sport_type("strength_training"), "WeightTraining");
        // Spelled as Intervals.icu spells it, so the two copies of one session
        // are the same sport (seen on the rider's history: seven unfolded twins).
        assert_eq!(icu_sport_type("hiit"), "HighIntensityIntervalTraining");
        // Unknown keys keep their own name instead of becoming a ride.
        assert_eq!(icu_sport_type("pickleball"), "pickleball");
        assert!(!is_cycling(&icu_sport_type("")));
    }

    #[test]
    fn should_wait_out_the_window_garmin_names_plus_one_second() {
        let body = r#"{"count":101,"limit":100,"retryInSeconds":17,"rateLimitMetaData":1}"#;
        assert_eq!(retry_after(body).as_secs(), 18);
        // The window just ended: still wait a second, never zero.
        assert_eq!(retry_after(r#"{"retryInSeconds":0}"#).as_secs(), 1);
        // No figure, or not JSON: a whole minute.
        assert_eq!(retry_after("Too Many Requests").as_secs(), 61);
        assert_eq!(retry_after(r#"{"retryInSeconds":-5}"#).as_secs(), 61);
        // A hostile figure cannot stall the import for a day.
        assert_eq!(retry_after(r#"{"retryInSeconds":86400}"#).as_secs(), 301);
    }

    #[test]
    fn should_discard_negative_and_non_finite_readings_instead_of_wrapping() {
        assert_eq!(whole(Some(-1.0)), None);
        assert_eq!(whole(Some(f64::NAN)), None);
        assert_eq!(whole(Some(1e300)), Some(u32::MAX));
        assert_eq!(whole(Some(0.5)), Some(1));
    }

    #[test]
    fn should_merge_three_endpoints_into_one_day() {
        let mut days = BTreeMap::new();
        merge_hrv(
            &mut days,
            r#"{"hrvSummaries":[{"calendarDate":"2026-10-20","lastNightAvg":48,"weeklyAvg":51}]}"#,
        )
        .unwrap();
        merge_sleep(
            &mut days,
            r#"[{"calendarDate":"2026-10-20","sleepTimeSeconds":26820,
                 "sleepScores":{"overall":{"value":82,"qualifierKey":"GOOD"}}}]"#,
        )
        .unwrap();
        merge_summaries(
            &mut days,
            r#"{"data":[{"calendarDate":"2026-10-20","heartRate":{"restingValue":47},
                 "movement":{"steps":{"goal":8000,"value":10234}},
                 "calories":{"burnedTotal":2710}}]}"#,
        )
        .unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let e = &days[&d];
        assert_eq!(e.hrv, Some(48.0));
        assert_eq!(e.sleep_secs, Some(26820));
        assert_eq!(e.sleep_score, Some(82));
        assert_eq!(e.resting_hr, Some(47));
        assert_eq!(e.steps, Some(10234));
        assert_eq!(e.calories, Some(2710));
    }

    #[test]
    fn should_not_report_a_zero_hrv_or_zero_sleep_as_a_reading() {
        let mut days = BTreeMap::new();
        merge_hrv(
            &mut days,
            r#"{"hrvSummaries":[{"calendarDate":"2026-10-20","lastNightAvg":0}]}"#,
        )
        .unwrap();
        merge_sleep(
            &mut days,
            r#"[{"calendarDate":"2026-10-20","sleepTimeSeconds":0}]"#,
        )
        .unwrap();
        let e = days.values().next().unwrap();
        assert_eq!(e.hrv, None);
        assert_eq!(e.sleep_secs, None);
        assert_eq!(e.sleep_score, None);
    }

    #[test]
    fn should_cover_a_range_in_chunks_with_no_gap_or_overlap() {
        let a = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap();
        let b = NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
        let c = chunks(a, b);
        assert_eq!(c.first().unwrap().0, a);
        assert_eq!(c.last().unwrap().1, b);
        for w in c.windows(2) {
            assert_eq!(w[0].1 + Duration::days(1), w[1].0);
        }
        assert!(c
            .iter()
            .all(|(f, t)| (*t - *f).num_days() < RANGE_CHUNK_DAYS));
        // A single day is one chunk; an inverted range is none.
        assert_eq!(chunks(a, a), [(a, a)]);
        assert!(chunks(b, a).is_empty());
    }

    #[test]
    fn should_pass_a_bare_fit_through_and_reject_anything_else() {
        let mut fit = vec![14, 0x20, 0, 0, 0, 0, 0, 0];
        fit.extend_from_slice(b".FIT");
        fit.extend_from_slice(&[0, 0]);
        assert_eq!(extract_fit(&fit).unwrap(), fit);
        assert!(extract_fit(b"<html>").is_err());
        assert!(extract_fit(b"").is_err());
        // A zip signature on a truncated archive is an error, not a panic.
        assert!(extract_fit(b"PK\x03\x04garbage").is_err());
    }

    #[test]
    fn should_find_the_fit_inside_a_zip_and_ignore_other_entries() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("readme.txt", opts).unwrap();
            w.write_all(b"not this").unwrap();
            w.start_file("21234567890_ACTIVITY.FIT", opts).unwrap();
            w.write_all(b"fit-bytes").unwrap();
            w.finish().unwrap();
        }
        assert_eq!(extract_fit(buf.get_ref()).unwrap(), b"fit-bytes");
    }
}
