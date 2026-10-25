//! Garmin vs Intervals.icu, side by side — a harness, not a test.
//!
//! It calls both live APIs with the rider's keys from the keyring, so it is
//! `#[ignore]`d and never runs in CI. Nothing is written to any database; the
//! only output is a Markdown report.
//!
//! ```text
//! ICU_ATHLETE_ID=i12345 GARMIN_REPORT=/path/report.md \
//!   cargo test --release garmin_vs_icu -- --ignored --nocapture
//! ```

use super::*;
use crate::ai::intervals;
use crate::data::keystore;
use std::fmt::Write as _;
use std::time::Instant;

const WINDOW_DAYS: i64 = 60;
/// How far apart two start times may be and still be the same ride. Garmin
/// and Intervals.icu read the same file, so they should agree to the second.
const MATCH_SECS: i64 = 120;
const STREAM_SAMPLES: usize = 3;

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "—".into())
}

fn km(v: Option<f32>) -> Option<String> {
    v.map(|m| format!("{:.2}", m / 1000.0))
}

fn pair<T: std::fmt::Display>(icu: Option<T>, garmin: Option<T>) -> String {
    format!("{} / {}", opt(icu), opt(garmin))
}

/// Tally one field across matched rides: how often each side has it, and how
/// often both agree within `tol`.
#[derive(Default)]
struct Tally {
    icu: usize,
    garmin: usize,
    both: usize,
    agree: usize,
    worst: f64,
}

impl Tally {
    fn add(&mut self, icu: Option<f64>, garmin: Option<f64>, tol: f64) {
        self.icu += icu.is_some() as usize;
        self.garmin += garmin.is_some() as usize;
        if let (Some(a), Some(b)) = (icu, garmin) {
            self.both += 1;
            let d = (a - b).abs();
            self.agree += (d <= tol) as usize;
            self.worst = self.worst.max(d);
        }
    }
}

fn start_of(a: &ActivitySummary) -> Option<NaiveDateTime> {
    a.start_datetime_local
}

/// Raw headers that look like rate limiting, from one search request.
async fn rate_headers(token: &AccessToken, today: NaiveDate) -> Result<Vec<String>> {
    let response = make_client()?
        .get(format!(
            "{BASE_URL}/activitylist-service/activities/search/activities\
             ?startDate={today}&endDate={today}&limit=1"
        ))
        .bearer_auth(&token.0)
        .send()
        .await?;
    Ok(response
        .headers()
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k.contains("rate") || k.contains("limit") || k.contains("retry") || k.contains("quota")
        })
        .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("?")))
        .collect())
}

/// What one recording carries, channel by channel.
struct Channels {
    samples: usize,
    power: usize,
    hr: usize,
    cadence: usize,
    gps: usize,
    altitude: usize,
}

fn fit_channels(fit: &[u8], id: &str) -> Result<Channels> {
    // import_fit_file reads from disk and carries the header guard that keeps
    // fitparser from panicking on a truncated file.
    let path = std::env::temp_dir().join(format!("garmin-compare-{id}.fit"));
    std::fs::write(&path, fit)?;
    let session = crate::data::fit::import_fit_file(&path);
    let _ = std::fs::remove_file(&path);
    let p = session?.data_points;
    Ok(Channels {
        samples: p.len(),
        power: p.iter().filter(|d| d.power_watts.is_some()).count(),
        hr: p.iter().filter(|d| d.heart_rate_bpm.is_some()).count(),
        cadence: p.iter().filter(|d| d.cadence_rpm.is_some()).count(),
        gps: p.iter().filter(|d| d.lat.is_some()).count(),
        altitude: p.iter().filter(|d| d.altitude_m.is_some()).count(),
    })
}

fn stream_channels(s: &crate::data::streams::ActivityStreams) -> Channels {
    Channels {
        samples: s.time_s.len(),
        power: s.watts.len(),
        hr: s.heartrate.len(),
        cadence: s.cadence.len(),
        gps: s.latlng.len(),
        altitude: s.altitude_m.len(),
    }
}

#[tokio::test]
#[ignore = "calls the live Garmin and Intervals.icu APIs with the rider's keys"]
async fn garmin_vs_icu() {
    let athlete_id = std::env::var("ICU_ATHLETE_ID").expect("set ICU_ATHLETE_ID");
    let report_path = std::env::var("GARMIN_REPORT").unwrap_or_else(|_| "garmin_vs_icu.md".into());
    let garmin_key = keystore::get_secret(keystore::KEY_GARMIN_API)
        .unwrap()
        .expect("no Garmin key in the keyring — enter it in Preferences → Integrations");
    let icu_key = keystore::get_secret(keystore::KEY_INTERVALS_API)
        .unwrap()
        .expect("no Intervals.icu key in the keyring");

    let today = chrono::Local::now().date_naive();
    let oldest = today - Duration::days(WINDOW_DAYS - 1);
    let mut r = String::new();
    let _ = writeln!(r, "# Garmin Personal API vs Intervals.icu\n");
    let _ = writeln!(
        r,
        "Window: {oldest} → {today} ({WINDOW_DAYS} days). Run {}.\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );

    // ── Auth ──
    let t = Instant::now();
    let token = fetch_token(&garmin_key).await.expect("Garmin token");
    let _ = writeln!(r, "## Operational\n");
    let _ = writeln!(r, "- Garmin token exchange: {} ms", t.elapsed().as_millis());
    match rate_headers(&token, today).await {
        Ok(h) if h.is_empty() => {
            let _ = writeln!(r, "- Garmin rate-limit headers: none sent");
        }
        Ok(h) => {
            let _ = writeln!(r, "- Garmin rate-limit headers: `{}`", h.join("`, `"));
        }
        Err(e) => {
            let _ = writeln!(r, "- Garmin rate-limit probe failed: {e}");
        }
    }

    // ── Activities ──
    let t = Instant::now();
    let garmin = fetch_activities(&token, oldest, today)
        .await
        .expect("Garmin activities");
    let garmin_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let icu = intervals::fetch_activities(&athlete_id, &icu_key, oldest, today)
        .await
        .expect("icu activities");
    let icu_ms = t.elapsed().as_millis();
    let _ = writeln!(r, "- Activities fetch: Garmin {garmin_ms} ms ({} rides), Intervals.icu {icu_ms} ms ({} rides)", garmin.len(), icu.len());

    let t = Instant::now();
    let garmin_well = fetch_wellness(&token, oldest, today).await;
    let gw_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let icu_well = intervals::fetch_wellness(&athlete_id, &icu_key, oldest, today).await;
    let iw_ms = t.elapsed().as_millis();
    let _ = writeln!(
        r,
        "- Wellness fetch: Garmin {gw_ms} ms ({} calls), Intervals.icu {iw_ms} ms (1 call)",
        chunks(oldest, today).len() * 3
    );

    // History depth: does Garmin answer for a year and two years back?
    for years in [1, 2, 5] {
        let to = today - Duration::days(365 * years);
        let from = to - Duration::days(29);
        let n = fetch_activities(&token, from, to).await;
        let w = fetch_wellness(&token, from, to).await;
        let _ = writeln!(
            r,
            "- History {years} y back ({from} → {to}): {} activities, {} wellness days",
            n.as_ref()
                .map(|v| v.len().to_string())
                .unwrap_or_else(|e| format!("error: {e}")),
            w.as_ref()
                .map(|v| v
                    .iter()
                    .filter(|d| d.hrv.is_some() || d.resting_hr.is_some() || d.sleep_secs.is_some())
                    .count()
                    .to_string())
                .unwrap_or_else(|e| format!("error: {e}")),
        );
    }

    // Match each icu ride to the Garmin ride that started closest to it.
    let mut used = vec![false; garmin.len()];
    let mut matched: Vec<(&ActivitySummary, &GarminActivity)> = Vec::new();
    let mut icu_only = Vec::new();
    for a in &icu {
        let best = start_of(a).and_then(|s| {
            garmin
                .iter()
                .enumerate()
                .filter(|(i, _)| !used[*i])
                .filter_map(|(i, g)| {
                    let d = (start_of(&g.summary)? - s).num_seconds().abs();
                    (d <= MATCH_SECS).then_some((d, i))
                })
                .min()
        });
        match best {
            Some((_, i)) => {
                used[i] = true;
                matched.push((a, &garmin[i]));
            }
            None => icu_only.push(a),
        }
    }
    let garmin_only: Vec<_> = garmin
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(g, _)| g)
        .collect();

    let _ = writeln!(r, "\n## Activities\n");
    let _ = writeln!(
        r,
        "{} matched · {} only in Intervals.icu · {} only in Garmin\n",
        matched.len(),
        icu_only.len(),
        garmin_only.len()
    );

    let mut tallies: Vec<(&str, Tally)> = [
        "Moving time (s, ±5)",
        "Avg power (W, ±2)",
        "NP (W, ±3)",
        "Avg HR (±1)",
        "Max HR (±1)",
        "Cadence (±1)",
        "Distance (m, ±50)",
        "Elevation (m, ±10)",
        "Load: icu load vs Garmin TSS (±5)",
    ]
    .into_iter()
    .map(|n| (n, Tally::default()))
    .collect();
    let f = |v: Option<u32>| v.map(f64::from);
    let g = |v: Option<f32>| v.map(f64::from);
    for (a, b) in &matched {
        let s = &b.summary;
        let rows = [
            (f(a.moving_time), f(s.moving_time), 5.0),
            (f(a.average_watts), f(s.average_watts), 2.0),
            (f(a.normalized_watts), f(s.normalized_watts), 3.0),
            (f(a.average_hr), f(s.average_hr), 1.0),
            (f(a.max_hr), f(s.max_hr), 1.0),
            (g(a.average_cadence), g(s.average_cadence), 1.0),
            (g(a.distance_m), g(s.distance_m), 50.0),
            (g(a.elevation_gain_m), g(s.elevation_gain_m), 10.0),
            (g(a.icu_training_load), g(b.training_stress_score), 5.0),
        ];
        for ((_, t), (x, y, tol)) in tallies.iter_mut().zip(rows) {
            t.add(x, y, tol);
        }
    }
    let _ = writeln!(
        r,
        "| Field | icu has | Garmin has | both | agree | worst gap |\n|---|---|---|---|---|---|"
    );
    for (n, t) in &tallies {
        let _ = writeln!(
            r,
            "| {n} | {} | {} | {} | {} | {:.1} |",
            t.icu, t.garmin, t.both, t.agree, t.worst
        );
    }

    let _ = writeln!(r, "\n### Matched rides (icu / Garmin)\n");
    let _ = writeln!(r, "| Date | Name (icu) | Type | Moving s | Avg W | NP | Avg HR | Cad | km | Load / TSS |\n|---|---|---|---|---|---|---|---|---|---|");
    for (a, b) in &matched {
        let s = &b.summary;
        let _ = writeln!(
            r,
            "| {} | {} | {} / {} | {} | {} | {} | {} | {} | {} | {} |",
            a.start_date_local,
            a.name,
            a.sport_type,
            s.sport_type,
            pair(a.moving_time, s.moving_time),
            pair(a.average_watts, s.average_watts),
            pair(a.normalized_watts, s.normalized_watts),
            pair(a.average_hr, s.average_hr),
            pair(
                a.average_cadence.map(|c| c.round()),
                s.average_cadence.map(|c| c.round())
            ),
            pair(km(a.distance_m), km(s.distance_m)),
            pair(
                a.icu_training_load.map(|l| l.round()),
                b.training_stress_score.map(|l| l.round())
            ),
        );
    }
    let _ = writeln!(r, "\n### Only in Intervals.icu\n");
    for a in &icu_only {
        let _ = writeln!(
            r,
            "- {} {} ({}, {} s, avg {} W)",
            opt(start_of(a)),
            a.name,
            a.sport_type,
            opt(a.moving_time),
            opt(a.average_watts)
        );
    }
    let _ = writeln!(r, "\n### Only in Garmin\n");
    for b in &garmin_only {
        let s = &b.summary;
        let _ = writeln!(
            r,
            "- {} {} ({}, {} s, avg {} W)",
            opt(start_of(s)),
            s.name,
            s.sport_type,
            opt(s.moving_time),
            opt(s.average_watts)
        );
    }

    // ── Streams vs FIT ──
    let _ = writeln!(r, "\n## Per-second data: icu streams vs Garmin FIT\n");
    let _ = writeln!(r, "| Ride | Source | samples | power | HR | cadence | GPS | altitude | fetch ms |\n|---|---|---|---|---|---|---|---|---|");
    // Outdoor rides recorded on the Garmin first: they are the data an
    // Intervals.icu block would take away. The app's own rides come after.
    let outdoor = |b: &GarminActivity| {
        let t = &b.summary.sport_type;
        b.has_gps && !t.contains("virtual") && !t.contains("indoor")
    };
    let mut order: Vec<_> = matched.iter().collect();
    order.sort_by_key(|(_, b)| (!outdoor(b), !b.has_gps));
    let sampled = order.into_iter().take(STREAM_SAMPLES);
    for (a, b) in sampled {
        let label = format!("{} {}", a.start_date_local, a.name);
        let row = |r: &mut String, src: &str, c: Result<Channels>, ms: u128| match c {
            Ok(c) => {
                let _ = writeln!(
                    r,
                    "| {label} | {src} | {} | {} | {} | {} | {} | {} | {ms} |",
                    c.samples, c.power, c.hr, c.cadence, c.gps, c.altitude
                );
            }
            Err(e) => {
                let _ = writeln!(r, "| {label} | {src} | error: {e} | | | | | | {ms} |");
            }
        };
        let t = Instant::now();
        let icu_s = intervals::fetch_streams_for_comparison(&icu_key, &a.id)
            .await
            .and_then(|s| s.map(|s| stream_channels(&s)).context("no streams"));
        row(&mut r, "icu", icu_s, t.elapsed().as_millis());
        let t = Instant::now();
        let fit = fetch_activity_fit(&token, &b.summary.id)
            .await
            .and_then(|bytes| fit_channels(&bytes, &b.summary.id));
        row(&mut r, "Garmin FIT", fit, t.elapsed().as_millis());
    }

    // ── Wellness ──
    let _ = writeln!(r, "\n## Wellness\n");
    match (&icu_well, &garmin_well) {
        (Ok(iw), Ok(gw)) => {
            let by_day = |v: &Vec<WellnessEntry>| {
                v.iter()
                    .map(|e| (e.date, e.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            let (iw, gw) = (by_day(iw), by_day(gw));
            let mut cov: Vec<(&str, Tally)> = [
                "HRV (ms, ±1)",
                "Resting HR (±1)",
                "Sleep (s, ±300)",
                "Sleep score (±1)",
                "Steps (±50)",
            ]
            .into_iter()
            .map(|n| (n, Tally::default()))
            .collect();
            let mut d = oldest;
            let mut rows = String::new();
            while d <= today {
                let (a, b) = (iw.get(&d), gw.get(&d));
                let pick = |e: Option<&WellnessEntry>| {
                    (
                        e.and_then(|e| e.hrv).map(f64::from),
                        e.and_then(|e| e.resting_hr).map(f64::from),
                        e.and_then(|e| e.sleep_secs).map(f64::from),
                        e.and_then(|e| e.sleep_score).map(f64::from),
                        e.and_then(|e| e.steps).map(f64::from),
                    )
                };
                let (x, y) = (pick(a), pick(b));
                cov[0].1.add(x.0, y.0, 1.0);
                cov[1].1.add(x.1, y.1, 1.0);
                cov[2].1.add(x.2, y.2, 300.0);
                cov[3].1.add(x.3, y.3, 1.0);
                cov[4].1.add(x.4, y.4, 50.0);
                let _ = writeln!(
                    rows,
                    "| {d} | {} | {} | {} | {} | {} |",
                    pair(x.0, y.0),
                    pair(x.1, y.1),
                    pair(x.2, y.2),
                    pair(x.3, y.3),
                    pair(x.4, y.4)
                );
                d += Duration::days(1);
            }
            let _ = writeln!(r, "| Field | icu days | Garmin days | both | agree | worst gap |\n|---|---|---|---|---|---|");
            for (n, t) in &cov {
                let _ = writeln!(
                    r,
                    "| {n} | {} | {} | {} | {} | {:.1} |",
                    t.icu, t.garmin, t.both, t.agree, t.worst
                );
            }
            let _ = writeln!(r, "\n### Per day (icu / Garmin)\n\n| Date | HRV | Resting HR | Sleep s | Score | Steps |\n|---|---|---|---|---|---|\n{rows}");
        }
        (i, g) => {
            let _ = writeln!(
                r,
                "- Intervals.icu: {}",
                i.as_ref()
                    .map(|v| format!("{} days", v.len()))
                    .unwrap_or_else(|e| format!("error: {e}"))
            );
            let _ = writeln!(
                r,
                "- Garmin: {}",
                g.as_ref()
                    .map(|v| format!("{} days", v.len()))
                    .unwrap_or_else(|e| format!("error: {e}"))
            );
        }
    }

    std::fs::write(&report_path, &r).expect("write report");
    println!("{r}\nReport written to {report_path}");
}

/// Can a five-year-old activity file still be downloaded and read?
#[tokio::test]
#[ignore = "calls the live Garmin API with the rider's key"]
async fn garmin_old_fit() {
    let key = keystore::get_secret(keystore::KEY_GARMIN_API)
        .unwrap()
        .expect("no Garmin key");
    let token = fetch_token(&key).await.expect("Garmin token");
    let from = NaiveDate::from_ymd_opt(2021, 9, 26).expect("hardcoded valid date");
    let to = NaiveDate::from_ymd_opt(2021, 10, 25).expect("hardcoded valid date");
    let list = fetch_activities(&token, from, to)
        .await
        .expect("activities");
    for a in &list {
        let s = &a.summary;
        println!(
            "{} {} [{}] gps={} {} s avg {} W",
            opt(s.start_datetime_local),
            s.name,
            s.sport_type,
            a.has_gps,
            opt(s.moving_time),
            opt(s.average_watts)
        );
    }
    // Rides first, outdoor before indoor, then anything else.
    let mut order: Vec<_> = list.iter().collect();
    order.sort_by_key(|a| {
        (
            !a.summary.sport_type.contains("cycl") && !a.summary.sport_type.contains("biking"),
            !a.has_gps,
        )
    });
    for a in order.into_iter().take(2) {
        let id = &a.summary.id;
        let t = Instant::now();
        let fit = fetch_activity_fit(&token, id).await;
        let ms = t.elapsed().as_millis();
        match fit {
            Ok(bytes) => {
                let len = bytes.len();
                match fit_channels(&bytes, id) {
                    Ok(c) => println!(
                        "\nFIT {id} ({}, {}): {len} bytes in {ms} ms — {} samples, power {}, HR {}, cadence {}, GPS {}, altitude {}",
                        a.summary.start_date_local, a.summary.sport_type, c.samples, c.power, c.hr, c.cadence, c.gps, c.altitude
                    ),
                    Err(e) => println!("\nFIT {id}: {len} bytes downloaded but unreadable: {e}"),
                }
            }
            Err(e) => println!("\nFIT {id}: download failed after {ms} ms: {e}"),
        }
    }
}
