use super::*;
use crate::data::db::testing::test_pool;
use crate::data::session::{DataPoint, Session};
use chrono::{Local, NaiveDateTime, Utc};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Mutex;

// ── A fake Garmin ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Fake {
    activities: Vec<GarminActivity>,
    wellness: Vec<WellnessEntry>,
    /// Garmin id → the file, or the status Garmin refuses it with.
    files: HashMap<String, Result<Vec<u8>, u16>>,
    earliest: Option<NaiveDate>,
    /// Refuse every list request with this status.
    refuse_lists: Option<u16>,
    /// Refuse only the wellness requests with this status.
    refuse_wellness: Option<u16>,
    fits_asked: Mutex<Vec<String>>,
    activity_windows: Mutex<Vec<(NaiveDate, NaiveDate)>>,
    wellness_windows: Mutex<Vec<(NaiveDate, NaiveDate)>>,
}

fn refusal(status: u16) -> anyhow::Error {
    super::super::refused(
        reqwest::StatusCode::from_u16(status).expect("valid test status"),
        "a test request",
        String::new(),
    )
}

impl Source for Fake {
    fn activities(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<GarminActivity>>> + Send {
        self.activity_windows.lock().unwrap().push((oldest, newest));
        let r = match self.refuse_lists {
            Some(s) => Err(refusal(s)),
            None => Ok(self
                .activities
                .iter()
                .filter(|a| (oldest..=newest).contains(&a.summary.start_date_local))
                .cloned()
                .collect()),
        };
        async move { r }
    }

    fn wellness(
        &self,
        oldest: NaiveDate,
        newest: NaiveDate,
    ) -> impl Future<Output = Result<Vec<WellnessEntry>>> + Send {
        self.wellness_windows.lock().unwrap().push((oldest, newest));
        let r = match self.refuse_lists.or(self.refuse_wellness) {
            Some(s) => Err(refusal(s)),
            None => Ok(self
                .wellness
                .iter()
                .filter(|w| (oldest..=newest).contains(&w.date))
                .cloned()
                .collect()),
        };
        async move { r }
    }

    fn fit(&self, garmin_id: &str) -> impl Future<Output = Result<Vec<u8>>> + Send {
        self.fits_asked.lock().unwrap().push(garmin_id.to_string());
        let r = match self.files.get(garmin_id) {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(status)) => Err(refusal(*status)),
            None => Err(anyhow::anyhow!("connection reset")),
        };
        async move { r }
    }

    fn earliest(&self) -> impl Future<Output = Result<Option<NaiveDate>>> + Send {
        let r = Ok(self.earliest);
        async move { r }
    }
}

// ── Builders ─────────────────────────────────────────────────────────────────

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).expect("hardcoded valid date")
}

fn at(d: NaiveDate, h: u32) -> NaiveDateTime {
    d.and_hms_opt(h, 0, 0).expect("hardcoded valid time")
}

/// A Garmin activity. Its own TSS is a figure no formula here would give, so
/// a test can tell it was not used.
fn activity(
    id: &str,
    sport: &str,
    start: NaiveDateTime,
    np: Option<u32>,
    set_ftp: Option<u32>,
) -> GarminActivity {
    GarminActivity {
        summary: ActivitySummary {
            id: id.into(),
            name: "Morning Ride".into(),
            start_date_local: start.date(),
            start_datetime_local: Some(start),
            icu_training_load: None,
            moving_time: Some(3600),
            average_watts: np,
            normalized_watts: np,
            average_hr: Some(140),
            max_hr: Some(170),
            sport_type: sport.into(),
            distance_m: None,
            elevation_gain_m: None,
            average_cadence: None,
        },
        training_stress_score: Some(999.0),
        has_gps: false,
        set_ftp,
    }
}

fn wellness(date: NaiveDate, resting_hr: u32) -> WellnessEntry {
    WellnessEntry {
        date,
        hrv: Some(50.0),
        resting_hr: Some(resting_hr),
        sleep_secs: None,
        sleep_score: None,
        steps: None,
        calories: None,
    }
}

/// A real FIT file: half an hour of heart rate climbing 110 → 169 bpm.
fn hr_fit() -> Vec<u8> {
    let mut s = Session::new(None);
    s.started_at = Utc::now() - chrono::Duration::days(3);
    s.ended_at = Some(s.started_at + chrono::Duration::seconds(1800));
    s.data_points = (0..1800)
        .map(|i| DataPoint {
            elapsed_secs: i,
            power_watts: None,
            target_watts: None,
            heart_rate_bpm: Some(110 + i / 30),
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        })
        .collect();
    crate::data::fit::encode_session(&s, &Default::default())
}

async fn stored(pool: &SqlitePool) -> Vec<db::IntervalsActivity> {
    db::load_intervals_activities(pool).await.unwrap()
}

async fn tss(pool: &SqlitePool, id: &str) -> Option<f32> {
    stored(pool)
        .await
        .into_iter()
        .find(|a| a.icu_id == id)
        .unwrap_or_else(|| panic!("{id} not stored"))
        .tss
}

fn stand_in_guard() -> ImportGuard {
    ImportGuard { held: false }
}

// ── FTP choice ───────────────────────────────────────────────────────────────

fn thresholds(history: Vec<(NaiveDate, u32)>, set: &[(NaiveDate, u32)]) -> Thresholds {
    let activities: Vec<_> = set
        .iter()
        .enumerate()
        .map(|(i, (d, w))| activity(&i.to_string(), "road_biking", at(*d, 8), None, Some(*w)))
        .collect();
    Thresholds::new(history, &activities, 211, 167, 185, 55)
}

#[test]
fn should_score_against_the_app_ftp_history_from_its_first_entry_on() {
    let t = thresholds(vec![(day(10), 230), (day(20), 240)], &[]);
    assert_eq!(t.ftp_on(day(10), Some(260)), 230);
    assert_eq!(t.ftp_on(day(19), Some(260)), 230);
    assert_eq!(t.ftp_on(day(20), Some(260)), 240);
    assert_eq!(t.ftp_on(day(31), None), 240);
    // The day before the history starts, the ride's own set FTP wins.
    assert_eq!(t.ftp_on(day(9), Some(260)), 260);
}

#[test]
fn should_take_garmins_200_w_default_as_no_ftp_at_all() {
    let t = thresholds(vec![], &[]);
    for unset in [198, 200, 202] {
        assert_eq!(t.ftp_on(day(9), Some(unset)), 211, "{unset}");
    }
    assert_eq!(t.ftp_on(day(9), Some(197)), 197);
    assert_eq!(t.ftp_on(day(9), Some(203)), 203);
    assert_eq!(t.ftp_on(day(9), Some(0)), 211);
}

#[test]
fn should_borrow_the_median_set_ftp_of_rides_within_45_days() {
    let base = NaiveDate::from_ymd_opt(2024, 6, 1).expect("hardcoded valid date");
    let set = [
        (base - Duration::days(45), 220),
        (base - Duration::days(10), 230),
        (base + Duration::days(10), 250),
        (base + Duration::days(45), 240),
        // One day too far, and Garmin's unset default: neither counts.
        (base + Duration::days(46), 400),
        (base, 200),
    ];
    let t = thresholds(vec![], &set);
    // 220, 230, 240, 250: of two middles, the upper.
    assert_eq!(t.ftp_on(base, None), 240);
    // Nothing within reach: the athlete's FTP.
    assert_eq!(t.ftp_on(base + Duration::days(200), None), 211);
}

#[test]
fn should_use_that_mornings_resting_heart_rate_unless_it_is_a_sensor_fault() {
    let t = thresholds(vec![], &[]);
    assert_eq!(t.hr(Some(44)).resting, 44);
    assert_eq!(t.hr(Some(26)).resting, 26);
    assert_eq!(t.hr(Some(25)).resting, 55);
    assert_eq!(t.hr(None).resting, 55);
    assert_eq!((t.hr(None).lthr, t.hr(None).max), (167, 185));
}

// ── Sync ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn should_store_garmin_activities_under_g_ids_spelled_as_icu_spells_them() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![activity("123", "road_biking", at(day(20), 8), None, None)],
        ..Fake::default()
    };
    let report = sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    assert_eq!(report.activities, 1);
    let a = &stored(&pool).await[0];
    assert_eq!(a.icu_id, "g123");
    assert_eq!(a.sport_type, "Ride");
}

#[tokio::test]
async fn should_score_a_powered_ride_itself_and_never_take_garmins_tss() {
    let pool = test_pool().await;
    let fake = Fake {
        // An hour at exactly the FTP the head unit held: 100.
        activities: vec![activity(
            "1",
            "road_biking",
            at(day(20), 8),
            Some(250),
            Some(250),
        )],
        ..Fake::default()
    };
    sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    assert_eq!(tss(&pool, "g1").await, Some(100.0));
}

#[tokio::test]
async fn should_rescore_a_powered_ride_when_the_ftp_behind_it_changes() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![activity(
            "1",
            "road_biking",
            at(day(20), 8),
            Some(250),
            Some(250),
        )],
        ..Fake::default()
    };
    sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    // The rider sets an FTP in the app that covers the ride's day.
    sqlx::query("INSERT INTO ftp_history (date, ftp_watts, source, note) VALUES (?, 500, 't', '')")
        .bind("2026-10-01")
        .execute(&pool)
        .await
        .unwrap();
    sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    // (250 / 500)² × 1 h × 100.
    assert_eq!(tss(&pool, "g1").await, Some(25.0));
}

#[tokio::test]
async fn should_leave_power_load_to_rides_and_heart_rate_to_everything_else() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![
            // Running power is not cycling power.
            activity("1", "running", at(day(20), 8), Some(250), Some(250)),
            // A ride without a power meter.
            activity("2", "road_biking", at(day(21), 8), None, None),
            // Zero normalised power is no power.
            activity("3", "road_biking", at(day(22), 8), Some(0), None),
        ],
        ..Fake::default()
    };
    sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    for id in ["g1", "g2", "g3"] {
        assert_eq!(tss(&pool, id).await, None, "{id}");
    }
}

#[tokio::test]
async fn should_ask_for_the_routine_windows() {
    let pool = test_pool().await;
    let fake = Fake::default();
    sync_recent(&pool, &fake, day(31)).await.unwrap();
    assert_eq!(*fake.activity_windows.lock().unwrap(), [(day(1), day(31))]);
    assert_eq!(*fake.wellness_windows.lock().unwrap(), [(day(24), day(31))]);
}

#[tokio::test]
async fn should_carry_on_without_wellness_but_not_without_the_key() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![activity("1", "road_biking", at(day(20), 8), None, None)],
        refuse_wellness: Some(500),
        ..Fake::default()
    };
    let report = sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    assert_eq!((report.activities, report.wellness), (1, 0));

    let refused = Fake {
        refuse_wellness: Some(401),
        ..Fake::default()
    };
    let err = sync(&pool, &refused, day(25), 30, 7).await.unwrap_err();
    assert!(is_unauthorized(&err));
    // Asked nothing more once the key was refused.
    assert!(refused.activity_windows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn should_fail_the_sync_when_the_activities_cannot_be_listed() {
    let pool = test_pool().await;
    let fake = Fake {
        refuse_lists: Some(503),
        ..Fake::default()
    };
    assert!(sync(&pool, &fake, day(25), 30, 7).await.is_err());
}

#[tokio::test]
async fn should_fold_the_icu_copy_into_the_garmin_one_on_the_first_sync() {
    let pool = test_pool().await;
    db::upsert_intervals_activity(
        &pool,
        "i9",
        day(20),
        "Morning Ride",
        Some(88.0),
        Some(3600),
        None,
        None,
        None,
        None,
        "Ride",
        Some(at(day(20), 8)),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let fake = Fake {
        activities: vec![activity("9", "road_biking", at(day(20), 8), None, None)],
        ..Fake::default()
    };
    let report = sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    assert_eq!(report.merged, 1);
    let left = stored(&pool).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].icu_id, "g9");
    // Without a file yet, the Intervals.icu load stands in.
    assert_eq!(left[0].tss, Some(88.0));
}

/// A ride recorded here three hours ago, an hour long.
async fn recorded_ride(pool: &SqlitePool) -> (i64, NaiveDateTime) {
    let mut s = Session::new(None);
    s.started_at = Utc::now() - chrono::Duration::hours(3);
    s.ended_at = Some(s.started_at + chrono::Duration::hours(1));
    let id = db::save_session(pool, &s).await.unwrap();
    (id, s.started_at.with_timezone(&Local).naive_local())
}

#[tokio::test]
async fn should_link_a_ride_uploaded_to_garmin_by_hand_to_the_ride_recorded_here() {
    let pool = test_pool().await;
    let (_, start) = recorded_ride(&pool).await;
    let fake = Fake {
        activities: vec![activity("5", "indoor_cycling", start, Some(200), None)],
        ..Fake::default()
    };
    sync(&pool, &fake, Local::now().date_naive(), 30, 7)
        .await
        .unwrap();
    assert!(db::linked_icu_ids(&pool).await.unwrap().contains("g5"));
}

#[tokio::test]
async fn should_link_a_hand_upload_whose_clock_is_three_hours_out() {
    let pool = test_pool().await;
    let (_, start) = recorded_ride(&pool).await;
    let fake = Fake {
        activities: vec![activity(
            "5",
            "indoor_cycling",
            start - Duration::hours(3),
            Some(200),
            None,
        )],
        ..Fake::default()
    };
    sync(&pool, &fake, Local::now().date_naive(), 30, 7)
        .await
        .unwrap();
    assert!(db::linked_icu_ids(&pool).await.unwrap().contains("g5"));
}

// ── Files ────────────────────────────────────────────────────────────────────

async fn synced(pool: &SqlitePool, fake: &Fake) {
    sync(pool, fake, day(25), 30, 30).await.unwrap();
}

#[tokio::test]
async fn should_score_an_activity_without_power_from_its_file_and_that_mornings_rest() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![activity(
            "1",
            "strength_training",
            at(day(20), 8),
            None,
            None,
        )],
        wellness: vec![wellness(day(20), 44)],
        files: HashMap::from([("1".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!((report.fetched, report.scored), (1, 1));

    let points = crate::data::fit::import_fit_bytes(&hr_fit())
        .unwrap()
        .data_points;
    let with = |resting| HrThresholds {
        resting,
        lthr: 167,
        max: 185,
    };
    let expected = hrss(&points, with(44)).unwrap();
    assert_eq!(tss(&pool, "g1").await, Some(expected));
    // And it is not the figure the athlete's own resting rate gives.
    assert_ne!(Some(expected), hrss(&points, with(55)));
    let cached = db::get_activity_streams(&pool, "g1")
        .await
        .unwrap()
        .unwrap();
    let streams = crate::data::streams::ActivityStreams::from_json(&cached).unwrap();
    assert_eq!(streams.time_s.len(), 1800);
    assert_eq!(streams.heartrate.first(), Some(&110));
}

#[tokio::test]
async fn should_keep_a_power_load_when_the_file_arrives() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![activity(
            "1",
            "road_biking",
            at(day(20), 8),
            Some(250),
            Some(250),
        )],
        files: HashMap::from([("1".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!((report.fetched, report.scored), (1, 0));
    assert_eq!(tss(&pool, "g1").await, Some(100.0));
}

#[tokio::test]
async fn should_cache_a_lost_or_unreadable_file_as_empty_and_not_ask_again() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![
            activity("1", "road_biking", at(day(20), 8), None, None),
            activity("2", "road_biking", at(day(21), 8), None, None),
        ],
        files: HashMap::from([
            ("1".to_string(), Err(404)),
            ("2".to_string(), Ok(b"<html>not a fit</html>".to_vec())),
        ]),
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!((report.fetched, report.missing), (0, 2));
    for id in ["g1", "g2"] {
        let cached = db::get_activity_streams(&pool, id).await.unwrap();
        assert_eq!(cached.as_deref(), Some("[]"), "{id}");
    }
    fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!(fake.fits_asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn should_stop_at_a_refused_key_and_cache_nothing_for_it() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![
            activity("1", "road_biking", at(day(20), 8), None, None),
            activity("2", "road_biking", at(day(21), 8), None, None),
        ],
        // Newest first: 2 is asked first, and refused.
        files: HashMap::from([("2".to_string(), Err(401)), ("1".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert!(report.unauthorized);
    assert_eq!(report.fetched, 0);
    assert_eq!(*fake.fits_asked.lock().unwrap(), ["2"]);
    assert_eq!(db::get_activity_streams(&pool, "g2").await.unwrap(), None);
}

#[tokio::test]
async fn should_stop_at_a_network_failure_and_try_that_file_again_next_time() {
    let pool = test_pool().await;
    let fake = Fake {
        activities: vec![
            activity("1", "road_biking", at(day(20), 8), None, None),
            // No file and no status: the connection drops.
            activity("2", "road_biking", at(day(21), 8), None, None),
        ],
        files: HashMap::from([("1".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!(
        report,
        FilesReport {
            stopped: true,
            ..FilesReport::default()
        }
    );
    assert_eq!(db::get_activity_streams(&pool, "g2").await.unwrap(), None);
    fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!(*fake.fits_asked.lock().unwrap(), ["2", "2"]);
}

#[tokio::test]
async fn should_download_at_most_a_batch_per_routine_pass_and_skip_linked_rides() {
    let pool = test_pool().await;
    let activities: Vec<_> = (1..=12)
        .map(|d| activity(&d.to_string(), "road_biking", at(day(d), 8), None, None))
        .collect();
    let files = (1..=12).map(|d| (d.to_string(), Ok(hr_fit()))).collect();
    let fake = Fake {
        activities,
        files,
        ..Fake::default()
    };
    synced(&pool, &fake).await;
    // The newest is a ride recorded here: it has its own samples.
    let session = db::save_session(&pool, &Session::new(None)).await.unwrap();
    db::set_session_icu_id(&pool, session, Some("g12"))
        .await
        .unwrap();

    let report = fetch_recent_files(&pool, &fake).await.unwrap();
    assert_eq!(report.fetched, FILE_BATCH);
    let asked = fake.fits_asked.lock().unwrap().clone();
    let expected: Vec<String> = (2..=11).rev().map(|d| d.to_string()).collect();
    assert_eq!(asked, expected);
}

// ── History import ───────────────────────────────────────────────────────────

#[tokio::test]
async fn should_import_nothing_from_an_account_with_no_activities() {
    let pool = test_pool().await;
    let fake = Fake::default();
    let report = import_history(
        &pool,
        &fake,
        day(25),
        std::time::Duration::ZERO,
        &stand_in_guard(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report, ImportReport::default());
    assert!(fake.wellness_windows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn should_import_every_day_and_activity_since_the_first_and_link_old_rides() {
    let pool = test_pool().await;
    // A ride recorded here a year ago: outside the routine 60-day link window.
    let mut old = Session::new(None);
    old.started_at = Utc::now() - chrono::Duration::days(365);
    old.ended_at = Some(old.started_at + chrono::Duration::hours(1));
    db::save_session(&pool, &old).await.unwrap();
    let old_start = old.started_at.with_timezone(&Local).naive_local();

    let today = Local::now().date_naive();
    let first = old_start.date() - Duration::days(30);
    let fake = Fake {
        earliest: Some(first),
        activities: vec![
            activity("1", "running", at(first, 8), None, None),
            activity("2", "indoor_cycling", old_start, Some(180), None),
        ],
        wellness: vec![wellness(first, 50), wellness(today, 48)],
        files: HashMap::from([("1".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    let seen = Mutex::new(Vec::new());
    let report = import_history(
        &pool,
        &fake,
        today,
        std::time::Duration::ZERO,
        &stand_in_guard(),
        |p| seen.lock().unwrap().push(p),
    )
    .await
    .unwrap();

    assert_eq!(report.activities, 2);
    assert_eq!(report.wellness, 2);
    assert_eq!(report.linked, 1);
    assert!(db::linked_icu_ids(&pool).await.unwrap().contains("g2"));
    // The linked ride's file is not fetched; the run's is, and scored.
    assert_eq!(report.files.fetched, 1);
    assert_eq!(report.files.scored, 1);
    assert_eq!(*fake.fits_asked.lock().unwrap(), ["1"]);

    // Wellness was asked for in chunks covering every day, first to today.
    let windows = fake.wellness_windows.lock().unwrap().clone();
    assert_eq!(windows.first().map(|w| w.0), Some(first));
    assert_eq!(windows.last().map(|w| w.1), Some(today));
    for w in windows.windows(2) {
        assert_eq!(w[0].1 + Duration::days(1), w[1].0);
    }
    assert_eq!(*fake.activity_windows.lock().unwrap(), [(first, today)]);

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.first(),
        Some(&Progress::Wellness {
            done: 0,
            total: windows.len()
        })
    );
    assert!(seen.contains(&Progress::Activities));
    assert_eq!(seen.last(), Some(&Progress::Files { done: 1, total: 1 }));
}

#[tokio::test]
async fn should_pick_up_an_interrupted_import_at_the_first_file_it_missed() {
    let pool = test_pool().await;
    let mut fake = Fake {
        earliest: Some(day(1)),
        activities: vec![
            activity("1", "road_biking", at(day(1), 8), None, None),
            activity("2", "road_biking", at(day(2), 8), None, None),
        ],
        // The newest is asked first, and the connection drops on the next.
        files: HashMap::from([("2".to_string(), Ok(hr_fit()))]),
        ..Fake::default()
    };
    async fn run(pool: &SqlitePool, fake: &Fake) -> ImportReport {
        let guard = stand_in_guard();
        import_history(
            pool,
            fake,
            day(25),
            std::time::Duration::ZERO,
            &guard,
            |_| {},
        )
        .await
        .unwrap()
    }
    assert_eq!(run(&pool, &fake).await.files.fetched, 1);
    fake.files.insert("1".to_string(), Ok(hr_fit()));
    fake.fits_asked.lock().unwrap().clear();
    assert_eq!(run(&pool, &fake).await.files.fetched, 1);
    assert_eq!(*fake.fits_asked.lock().unwrap(), ["1"]);
}

#[test]
fn should_refuse_a_second_import_while_one_runs_and_free_the_slot_after() {
    let first = try_begin_import().expect("no import running yet");
    assert!(try_begin_import().is_none());
    drop(first);
    let again = try_begin_import();
    assert!(again.is_some());
}

#[tokio::test]
async fn should_end_the_import_at_a_refused_key_but_ride_out_other_wellness_failures() {
    let pool = test_pool().await;
    let guard = stand_in_guard();
    let refused = Fake {
        earliest: Some(day(1)),
        refuse_wellness: Some(401),
        ..Fake::default()
    };
    let err = import_history(
        &pool,
        &refused,
        day(25),
        std::time::Duration::ZERO,
        &guard,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(is_unauthorized(&err));
    assert!(refused.activity_windows.lock().unwrap().is_empty());

    let flaky = Fake {
        earliest: Some(day(1)),
        refuse_wellness: Some(500),
        activities: vec![activity("1", "road_biking", at(day(2), 8), None, None)],
        ..Fake::default()
    };
    let report = import_history(
        &pool,
        &flaky,
        day(25),
        std::time::Duration::ZERO,
        &guard,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.activities, 1);
}

#[tokio::test]
async fn should_give_no_power_load_to_a_ride_with_no_moving_time() {
    let pool = test_pool().await;
    let mut a = activity("1", "road_biking", at(day(20), 8), Some(250), Some(250));
    a.summary.moving_time = Some(0);
    let fake = Fake {
        activities: vec![a],
        ..Fake::default()
    };
    sync(&pool, &fake, day(25), 30, 7).await.unwrap();
    // Not a load of 0: no load yet, so the file can still give one.
    assert_eq!(tss(&pool, "g1").await, None);
}

// ── Routine ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn should_tell_the_rider_when_garmin_refuses_the_key_at_sign_in() {
    let pool = test_pool().await;
    let notice = routine_with::<Fake>(&pool, Err(refusal(401)), day(25)).await;
    assert_eq!(notice.as_deref(), Some(UNAUTHORIZED_NOTICE));
    // A sign-in that merely failed says nothing: the network comes back.
    assert_eq!(
        routine_with::<Fake>(&pool, Err(anyhow::anyhow!("dns")), day(25)).await,
        None
    );
}

#[tokio::test]
async fn should_tell_the_rider_when_the_sync_itself_is_refused() {
    let pool = test_pool().await;
    let fake = Fake {
        refuse_lists: Some(401),
        ..Fake::default()
    };
    let notice = routine_with(&pool, Ok(fake), day(25)).await;
    assert_eq!(notice.as_deref(), Some(UNAUTHORIZED_NOTICE));
    let down = Fake {
        refuse_lists: Some(503),
        ..Fake::default()
    };
    assert_eq!(routine_with(&pool, Ok(down), day(25)).await, None);
}

#[tokio::test]
async fn should_sync_and_warn_of_a_key_about_to_expire() {
    let pool = test_pool().await;
    settings::set_garmin_key_expires(&pool, Some(day(27)))
        .await
        .unwrap();
    let fake = Fake {
        activities: vec![activity("1", "road_biking", at(day(20), 8), None, None)],
        ..Fake::default()
    };
    let notice = routine_with(&pool, Ok(fake), day(25)).await;
    assert_eq!(notice, expiry_notice(Some(day(27)), day(25)));
    assert!(notice.is_some());
    assert_eq!(stored(&pool).await.len(), 1);
    // Even with no way in, the expiry is still worth saying.
    let offline = routine_with::<Fake>(&pool, Err(anyhow::anyhow!("dns")), day(25)).await;
    assert_eq!(offline, notice);
}

// ── Notices ──────────────────────────────────────────────────────────────────

#[test]
fn should_warn_of_expiry_only_within_seven_days() {
    let today = day(10);
    let notice = |d: i64| expiry_notice(Some(today + Duration::days(d)), today);
    assert_eq!(notice(8), None);
    assert_eq!(
        notice(7).as_deref(),
        Some("Your Garmin API key expires in 7 days — make a new one in Garmin Connect")
    );
    assert!(notice(1).unwrap().contains("expires tomorrow"));
    assert!(notice(0).unwrap().contains("expires today"));
    assert_eq!(
        notice(-1).as_deref(),
        Some("Your Garmin API key expired on 9 October — make a new one in Garmin Connect")
    );
    assert_eq!(expiry_notice(None, today), None);
}
