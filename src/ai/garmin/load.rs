//! Work out a load for every Garmin activity in the test database, and grade
//! the result against the load Intervals.icu gave the same activity.
//!
//! A harness (`#[ignore]`): it reads the live Intervals.icu API and writes the
//! `tss` column of the `g`-prefixed rows in `<xdg>/cycle-dev/cycle.db` only.

use super::*;
use crate::data::keystore;
use chrono::Datelike;
use std::fmt::Write as _;

const ICU: &str = "https://intervals.icu/api/v1";

async fn icu_get(path: &str) -> serde_json::Value {
    let key = keystore::get_secret(keystore::KEY_INTERVALS_API)
        .unwrap()
        .expect("icu key");
    make_client()
        .unwrap()
        .get(format!("{ICU}{path}"))
        .basic_auth("API_KEY", Some(key))
        .send()
        .await
        .expect("icu request")
        .json()
        .await
        .expect("icu json")
}

/// Which load fields Intervals.icu exposes per activity, and the thresholds it
/// holds for the athlete — names only where a value could be personal.
#[tokio::test]
#[ignore = "calls the live Intervals.icu API"]
async fn icu_load_fields() {
    let id = std::env::var("ICU_ATHLETE_ID").expect("set ICU_ATHLETE_ID");
    let today = chrono::Local::now().date_naive();
    let acts = icu_get(&format!(
        "/athlete/{id}/activities?oldest={}&newest={today}",
        today - Duration::days(14)
    ))
    .await;
    let first = &acts[0];
    let mut keys: Vec<_> = first.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    println!("activity fields: {keys:?}");
    for k in [
        "type",
        "icu_training_load",
        "power_load",
        "hr_load",
        "pace_load",
        "hr_load_type",
        "icu_ftp",
        "lthr",
        "icu_resting_hr",
        "athlete_max_hr",
        "moving_time",
    ] {
        println!(
            "  {k} = {}",
            first.get(k).unwrap_or(&serde_json::Value::Null)
        );
    }
    let athlete = icu_get(&format!("/athlete/{id}")).await;
    for s in athlete["sportSettings"].as_array().into_iter().flatten() {
        println!(
            "sport settings {}: ftp {} lthr {} max_hr {} load_order {}",
            s["types"], s["ftp"], s["lthr"], s["max_hr"], s["load_order"]
        );
    }
}

/// What Intervals.icu said about one activity's load, and what it used.
struct IcuLoad {
    start: NaiveDateTime,
    training_load: Option<f64>,
    power_load: Option<f64>,
    hr_load: Option<f64>,
    ftp: Option<u32>,
    thresholds: Option<crate::training::load::HrThresholds>,
}

fn icu_loads(acts: &serde_json::Value) -> Vec<IcuLoad> {
    let num = |v: &serde_json::Value| v.as_f64();
    let whole = |v: &serde_json::Value| v.as_f64().filter(|x| *x > 0.0).map(|x| x.round() as u32);
    acts.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(IcuLoad {
                start: parse_local_time(a["start_date_local"].as_str()?)?,
                training_load: num(&a["icu_training_load"]),
                power_load: num(&a["power_load"]),
                hr_load: num(&a["hr_load"]),
                ftp: whole(&a["icu_ftp"]),
                thresholds: Some(crate::training::load::HrThresholds {
                    resting: whole(&a["icu_resting_hr"])?,
                    lthr: whole(&a["lthr"])?,
                    max: whole(&a["athlete_max_hr"])?,
                }),
            })
        })
        .collect()
}

/// The Intervals.icu record of the same activity: started within two minutes,
/// or within two minutes of a whole-hour offset — the app's own rides from
/// before 0.12.2 carry a local time three hours out.
fn find_match(icu: &[IcuLoad], start: NaiveDateTime) -> Option<&IcuLoad> {
    for offset_h in [0i64, -1, 1, -2, 2, -3, 3] {
        let s = start + Duration::hours(offset_h);
        if let Some(m) = icu
            .iter()
            .filter(|a| (a.start - s).num_seconds().abs() <= 120)
            .min_by_key(|a| (a.start - s).num_seconds().abs())
        {
            return Some(m);
        }
    }
    None
}

/// Per-second points back out of a cached stream (zero means "no reading").
fn points_of(json: &str) -> Vec<crate::data::session::DataPoint> {
    let Some(s) = crate::data::streams::ActivityStreams::from_json(json) else {
        return Vec::new();
    };
    let at = |v: &Vec<u32>, i: usize| v.get(i).copied().filter(|x| *x > 0);
    s.time_s
        .iter()
        .enumerate()
        .map(|(i, &t)| crate::data::session::DataPoint {
            elapsed_secs: t,
            power_watts: s.watts.get(i).copied(),
            target_watts: None,
            heart_rate_bpm: at(&s.heartrate, i),
            cadence_rpm: None,
            speed_kmh: None,
            lat: None,
            lng: None,
            altitude_m: None,
        })
        .collect()
}

/// Agreement between ours and Intervals.icu's figure for one kind of load.
#[derive(Default)]
struct Grade {
    pairs: Vec<(f64, f64)>,
}

impl Grade {
    fn report(&self, label: &str) -> String {
        let n = self.pairs.len();
        if n == 0 {
            return format!("| {label} | 0 | | | | |");
        }
        let mut pct: Vec<f64> = self
            .pairs
            .iter()
            .filter(|(_, icu)| *icu >= 10.0)
            .map(|(ours, icu)| (ours / icu - 1.0) * 100.0)
            .collect();
        pct.sort_by(f64::total_cmp);
        let q = |p: f64| {
            pct.get(((pct.len().max(1) - 1) as f64 * p) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let within = |tol: f64| {
            self.pairs
                .iter()
                .filter(|(o, i)| (o - i).abs() <= tol.max(i * 0.05))
                .count()
        };
        let sum_ours: f64 = self.pairs.iter().map(|p| p.0).sum();
        let sum_icu: f64 = self.pairs.iter().map(|p| p.1).sum();
        format!(
            "| {label} | {n} | {:+.1} % | {:+.1} % / {:+.1} % | {} ({:.0} %) | {:+.1} % |",
            q(0.5),
            q(0.1),
            q(0.9),
            within(3.0),
            within(3.0) as f64 / n as f64 * 100.0,
            (sum_ours / sum_icu - 1.0) * 100.0,
        )
    }
}

#[tokio::test]
#[ignore = "calls the live Intervals.icu API and writes to the cycle-dev database"]
async fn garmin_load() {
    use crate::training::load::{hrss, power_load, HrThresholds};
    let dir = crate::data::paths::data_dir();
    assert_eq!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("cycle-dev"),
        "refusing {}",
        dir.display()
    );
    let pool = crate::data::db::open()
        .await
        .expect("open cycle-dev database");
    let id = std::env::var("ICU_ATHLETE_ID").expect("set ICU_ATHLETE_ID");
    let today = chrono::Local::now().date_naive();

    let acts = icu_get(&format!(
        "/athlete/{id}/activities?oldest=2017-01-01&newest={today}&fields=start_date_local,\
         icu_training_load,power_load,hr_load,icu_ftp,lthr,icu_resting_hr,athlete_max_hr"
    ))
    .await;
    let icu = icu_loads(&acts);
    println!("Intervals.icu: {} activities", icu.len());

    // The app's own thresholds, for activities Intervals.icu has no record of:
    // the athlete's current LTHR and max, resting from that morning's wellness.
    let athlete = icu_get(&format!("/athlete/{id}")).await;
    let ride = athlete["sportSettings"]
        .as_array()
        .and_then(|s| {
            s.iter()
                .find(|s| s["types"].to_string().contains("\"Ride\""))
        })
        .cloned()
        .unwrap_or_default();
    let lthr = ride["lthr"].as_u64().unwrap_or(170) as u32;
    let max = ride["max_hr"].as_u64().unwrap_or(190) as u32;
    let resting_by_day: BTreeMap<NaiveDate, u32> =
        sqlx::query_as::<_, (String, Option<i64>)>("SELECT date, resting_hr FROM wellness_entries")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|(d, r)| Some((date_of(Some(&d))?, r.filter(|r| *r > 25)? as u32)))
            .collect();
    // FTP for a day with no Intervals.icu record: the last one it used before.
    let ftp_by_day: BTreeMap<NaiveDate, u32> = icu
        .iter()
        .filter_map(|a| Some((a.start.date(), a.ftp?)))
        .collect();

    // The FTP Garmin scored each ride against (NP ÷ IF, from garmin_ftp_fields),
    // without the 200 W it assumes when none was set.
    let mut set_ftp: BTreeMap<String, (NaiveDate, f64)> = BTreeMap::new();
    if let Ok(path) = std::env::var("GARMIN_FTP_CSV") {
        for line in std::fs::read_to_string(path)
            .expect("ftp csv")
            .lines()
            .skip(1)
        {
            let f: Vec<&str> = line.split(',').collect();
            let (Some(d), Ok(ftp)) = (date_of(f.first().copied()), f[2].parse::<f64>()) else {
                continue;
            };
            if !(198.0..=202.0).contains(&ftp) {
                set_ftp.insert(f[1].to_string(), (d, ftp));
            }
        }
    }
    let set_by_day: Vec<(NaiveDate, f64)> = {
        let mut v: Vec<_> = set_ftp.values().copied().collect();
        v.sort_by_key(|a| a.0);
        v
    };
    // This ride's own set FTP, else the median of those within 45 days.
    let history_ftp = |gid: &str, day: NaiveDate| -> Option<(u32, &'static str)> {
        if let Some((_, f)) = set_ftp.get(gid) {
            return Some((f.round() as u32, "own"));
        }
        let mut near: Vec<f64> = set_by_day
            .iter()
            .filter(|(d, _)| (*d - day).num_days().abs() <= 45)
            .map(|(_, f)| *f)
            .collect();
        near.sort_by(f64::total_cmp);
        near.get(near.len() / 2)
            .map(|f| (f.round() as u32, "nearby"))
    };
    let mut ftp_sources: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_year: BTreeMap<i32, (f64, f64)> = BTreeMap::new();

    let rows = sqlx::query_as::<_, (String, Option<String>, String, Option<i64>, Option<String>)>(
        "SELECT a.icu_id, a.start_datetime_local, a.sport_type, a.duration_secs, s.streams_json
         FROM intervals_activities a LEFT JOIN activity_streams s ON s.icu_id = a.icu_id
         WHERE a.icu_id LIKE 'g%'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    let (mut power_formula, mut hr_formula, mut chosen_icu_thr, mut chosen_own) = (
        Grade::default(),
        Grade::default(),
        Grade::default(),
        Grade::default(),
    );
    let (mut from_power, mut from_hr, mut none, mut no_file, mut matched) = (0, 0, 0, 0, 0);
    let mut by_sport_none: BTreeMap<String, usize> = BTreeMap::new();
    let mut worst: Vec<(f64, String)> = Vec::new();
    for (gid, start, sport_type, moving, json) in &rows {
        let Some(json) = json else {
            no_file += 1;
            *by_sport_none.entry(sport_type.clone()).or_default() += 1;
            continue;
        };
        let points = points_of(json);
        let Some(start) = start.as_deref().and_then(parse_local_time) else {
            continue;
        };
        let m = find_match(&icu, start);
        matched += m.is_some() as usize;
        let moving = moving.unwrap_or(0).max(0) as u32;
        let cycling = crate::data::sport::is_cycling(sport_type);
        let has_power = points
            .iter()
            .filter(|p| p.power_watts.unwrap_or(0) > 0)
            .count()
            * 2
            > points.len();

        let mut session = crate::data::session::Session::new(None);
        session.data_points = points.clone();
        let np = if has_power {
            session.normalised_power()
        } else {
            None
        };

        // 1. The formulas, fed exactly what Intervals.icu used.
        if let Some(m) = m {
            if let (Some(np), Some(ftp), Some(icu_p)) = (np, m.ftp, m.power_load) {
                if let Some(p) = power_load(np, moving, ftp) {
                    power_formula.pairs.push((p as f64, icu_p));
                }
            }
            if let (Some(t), Some(icu_h)) = (m.thresholds, m.hr_load) {
                if let Some(h) = hrss(&points, t) {
                    hr_formula.pairs.push((h as f64, icu_h));
                }
            }
        }

        // 2. The whole method, as the app would run it without Intervals.icu:
        // power first for rides, heart rate otherwise, own thresholds.
        let day = start.date();
        let flat_ftp = ftp_by_day
            .range(..=day)
            .next_back()
            .map(|(_, f)| *f)
            .unwrap_or(211);
        // The FTP history: the one Garmin scored the ride against, falling
        // back to the flat figure only where Garmin has none near the day.
        let (ftp, ftp_source) = history_ftp(gid, day).unwrap_or((flat_ftp, "flat"));
        let resting = resting_by_day.get(&day).copied().unwrap_or(52);
        let own = HrThresholds { resting, lthr, max };
        let power = if cycling {
            np.and_then(|np| power_load(np, moving, ftp))
        } else {
            None
        };
        if power.is_some() {
            *ftp_sources.entry(ftp_source).or_default() += 1;
        }
        let flat_power = if cycling {
            np.and_then(|np| power_load(np, moving, flat_ftp))
        } else {
            None
        };
        let hr_only = || hrss(&points, own);
        let year_entry = by_year.entry(day.year()).or_default();
        year_entry.0 += flat_power.or_else(hr_only).unwrap_or(0.0) as f64;
        year_entry.1 += power.or_else(hr_only).unwrap_or(0.0) as f64;
        let load = match power {
            Some(p) => {
                from_power += 1;
                Some(p)
            }
            None => hrss(&points, own).inspect(|_| from_hr += 1),
        };
        if load.is_none() {
            none += 1;
            *by_sport_none.entry(sport_type.clone()).or_default() += 1;
        }
        if let (Some(l), Some(icu_l)) = (load, m.and_then(|m| m.training_load)) {
            chosen_own.pairs.push((l as f64, icu_l));
            if icu_l >= 20.0 {
                worst.push((
                    ((l as f64) / icu_l - 1.0) * 100.0,
                    format!("{day} {sport_type} ours {l:.0} icu {icu_l:.0}"),
                ));
            }
            // And the same choice made with Intervals.icu's thresholds, to
            // separate formula error from threshold error.
            if let Some(m) = m {
                let p = if cycling {
                    np.zip(m.ftp).and_then(|(np, f)| power_load(np, moving, f))
                } else {
                    None
                };
                if let Some(l2) = p.or_else(|| m.thresholds.and_then(|t| hrss(&points, t))) {
                    chosen_icu_thr.pairs.push((l2 as f64, icu_l));
                }
            }
        }
        sqlx::query("UPDATE intervals_activities SET tss = ? WHERE icu_id = ?")
            .bind(load.map(f64::from))
            .bind(gid)
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut r = String::new();
    let _ = writeln!(r, "# Training load for Garmin activities\n");
    let _ = writeln!(r, "{} Garmin activities · {matched} matched to Intervals.icu · load from power {from_power}, from HR {from_hr}, none {none} (+{no_file} with no file)\n", rows.len());
    let _ = writeln!(r, "Own thresholds: LTHR {lthr}, max {max}, resting from that day's wellness (fallback 52), FTP the last Intervals.icu used on or before the day (fallback 211).\n");
    let _ = writeln!(r, "| Comparison | n | median gap | p10 / p90 | within ±3 or ±5 % | totals |\n|---|---|---|---|---|---|");
    let _ = writeln!(
        r,
        "{}",
        power_formula.report("Power load formula vs icu power_load (icu's FTP)")
    );
    let _ = writeln!(
        r,
        "{}",
        hr_formula.report("HRSS formula vs icu hr_load (icu's thresholds)")
    );
    let _ = writeln!(
        r,
        "{}",
        chosen_icu_thr.report("Chosen load vs icu load, icu's thresholds")
    );
    let _ = writeln!(
        r,
        "{}",
        chosen_own.report("Chosen load vs icu load, own thresholds")
    );
    let _ = writeln!(r, "\nNo load, by sport: {by_sport_none:?}\n");
    let _ = writeln!(r, "Power loads by FTP source: {ftp_sources:?}\n");
    let _ = writeln!(
        r,
        "| Year | load, flat FTP | load, FTP history | change |\n|---|---|---|---|"
    );
    for (y, (flat, hist)) in &by_year {
        let change = if *flat > 0.0 {
            (hist / flat - 1.0) * 100.0
        } else {
            0.0
        };
        let _ = writeln!(r, "| {y} | {flat:.0} | {hist:.0} | {change:+.1} % |");
    }
    let _ = writeln!(r);
    worst.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()));
    let _ = writeln!(r, "Largest gaps (own thresholds):");
    for (pct, s) in worst.iter().take(12) {
        let _ = writeln!(r, "- {pct:+.0} % {s}");
    }
    let path = std::env::var("GARMIN_REPORT").unwrap_or_else(|_| "garmin_load.md".into());
    std::fs::write(&path, &r).unwrap();
    println!("{r}\nReport written to {path}");
}

/// Does Garmin keep the FTP each ride was scored against? Per year: `maxFtp`
/// from the activity list, and NP / IF where both are present.
#[tokio::test]
#[ignore = "calls the live Garmin API"]
async fn garmin_ftp_fields() {
    let key = keystore::get_secret(keystore::KEY_GARMIN_API)
        .unwrap()
        .expect("no Garmin key");
    let token = fetch_token(&key).await.expect("Garmin token");
    // Per year: (date, maxFtp, NP ÷ IF) for each ride with normalised power.
    type Rides = Vec<(String, Option<f64>, Option<f64>)>;
    let mut per_year: BTreeMap<i32, Rides> = BTreeMap::new();
    let mut csv = String::from("date,gid,ftp,type,device\n");
    for page in 0.. {
        let body = get_text(
            &token,
            &format!(
                "/activitylist-service/activities/search/activities\
                 ?startDate=2020-06-01&endDate=2026-10-24&start={}&limit=99",
                page * 99
            ),
        )
        .await
        .unwrap()
        .unwrap_or_else(|| "[]".into());
        let v: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        for a in &v {
            let Some(np) = a["normPower"].as_f64() else {
                continue;
            };
            let year = a["startTimeLocal"]
                .as_str()
                .and_then(|s| s.get(..4)?.parse().ok())
                .unwrap_or(0);
            let from_if = a["intensityFactor"]
                .as_f64()
                .filter(|f| *f > 0.0)
                .map(|f| np / f);
            let date = a["startTimeLocal"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or("")
                .to_string();
            if let (Some(f), Some(id)) = (from_if, a["activityId"].as_i64()) {
                let kind = a["activityType"]["typeKey"].as_str().unwrap_or("");
                let device = a["deviceId"].as_i64().unwrap_or(0);
                csv.push_str(&format!("{date},g{id},{f:.1},{kind},{device}\n"));
            }
            per_year
                .entry(year)
                .or_default()
                .push((date, a["maxFtp"].as_f64(), from_if));
        }
        if v.len() < 99 {
            break;
        }
    }
    for (year, rides) in &per_year {
        let max_ftp: Vec<_> = rides.iter().filter_map(|r| r.1).collect();
        let derived: Vec<_> = rides
            .iter()
            .filter_map(|r| r.2.map(|f| f.round() as i64))
            .collect();
        let mut distinct: Vec<_> = derived.clone();
        distinct.sort();
        distinct.dedup();
        println!(
            "{year}: {} rides with NP · maxFtp on {} (values {:?}) · NP/IF on {} (distinct {:?})",
            rides.len(),
            max_ftp.len(),
            {
                let mut m: Vec<i64> = max_ftp.iter().map(|x| x.round() as i64).collect();
                m.sort();
                m.dedup();
                m
            },
            derived.len(),
            distinct
        );
    }
    if let Ok(path) = std::env::var("GARMIN_FTP_CSV") {
        std::fs::write(path, &csv).unwrap();
    }
    // When the derived FTP changes, in order.
    let mut last = None;
    for rides in per_year.values() {
        let mut rides = rides.clone();
        rides.sort_by(|a, b| a.0.cmp(&b.0));
        for (date, _, f) in rides {
            if let Some(f) = f.map(|f| f.round() as i64) {
                if last != Some(f) {
                    println!("  {date}: FTP {f}");
                    last = Some(f);
                }
            }
        }
    }
}
