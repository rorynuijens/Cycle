#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::Path;

// Physics model constants shared by `power_at` and `speed_from_power`.
const G: f32 = 9.81; // gravitational acceleration m/s²
const RHO: f32 = 1.22; // air density kg/m³
const CRR: f32 = 0.004; // rolling resistance coefficient
const CDA: f32 = 0.32; // combined drag area m²

/// A single point on a GPX route.
#[derive(Debug, Clone)]
pub struct RoutePoint {
    /// Latitude in degrees (WGS-84).
    pub lat: f64,
    /// Longitude in degrees (WGS-84).
    pub lng: f64,
    /// Elevation in metres (above sea level).
    pub elevation_m: f32,
    /// Cumulative distance from route start, in metres.
    pub distance_m: f32,
    /// Gradient to the *next* point (rise/run, fraction). 0.0 for the last point.
    pub gradient: f32,
}

/// A parsed GPX route ready for simulation.
#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub points: Vec<RoutePoint>,
    /// Total distance in metres.
    pub total_distance_m: f32,
    /// Total elevation gain in metres.
    pub total_gain_m: f32,
}

impl Route {
    /// Parse a GPX file from disk.
    pub fn from_gpx_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).context("failed to read GPX file")?;
        anyhow::ensure!(bytes.len() <= 5_242_880, "GPX file exceeds 5 MB");
        parse_gpx(&bytes)
    }

    /// Power required to ride at `speed_ms` (m/s) on a segment with the given `gradient`
    /// (fraction, e.g. 0.05 = 5% climb) at a combined mass of `mass_kg`.
    ///
    /// Uses a simplified road-cycling physics model:
    ///   P = (F_gravity + F_rolling + F_aero) × v
    pub fn power_at(speed_ms: f32, gradient: f32, mass_kg: f32) -> f32 {
        let f_gravity = mass_kg * G * gradient;
        let f_rolling = mass_kg * G * CRR;
        let f_aero = 0.5 * RHO * CDA * speed_ms * speed_ms;
        let power = (f_gravity + f_rolling + f_aero) * speed_ms;
        power.max(0.0)
    }

    /// Speed (m/s) sustained at `power_watts` on the given `gradient` — the inverse
    /// of [`Route::power_at`], used by SIM mode to derive virtual speed from the
    /// rider's actual power.
    ///
    /// Solves `P = (a + b·v²)·v` with `a = m·G·(gradient + Crr)` (which is negative
    /// on descents) and `b = ½·ρ·CdA`. On a descent at low power the cubic has
    /// multiple roots; the physical solution is the largest one, so bisection
    /// starts at the curve's stationary point where it is guaranteed monotonic.
    /// Zero power downhill therefore yields the coasting terminal speed.
    pub fn speed_from_power(power_watts: f32, gradient: f32, mass_kg: f32) -> f32 {
        let a = mass_kg * G * (gradient + CRR);
        let b = 0.5 * RHO * CDA;
        let f = |v: f32| (a + b * v * v) * v - power_watts;

        // Below the stationary point the curve can decrease; above it, it only rises.
        let mut lo = if a < 0.0 {
            (-a / (3.0 * b)).sqrt()
        } else {
            0.0
        };
        if f(lo) >= 0.0 {
            return lo; // already at/above the requested power at the minimum speed
        }
        let mut hi = 40.0; // 144 km/h — beyond any indoor-realistic speed
        for _ in 0..40 {
            let mid = (lo + hi) / 2.0;
            if f(mid) < 0.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }

    /// Build a per-second workout segment list for the route driven at constant FTP percentage.
    /// Returns `(distance_m, gradient, target_watts)` for each metre-granularity step
    /// (sub-sampled to ~1 point per 10 m for efficiency).
    pub fn workout_targets(&self, speed_ms: f32, mass_kg: f32) -> Vec<(f32, f32, u32)> {
        self.points
            .windows(2)
            .map(|w| {
                let watts = Self::power_at(speed_ms, w[0].gradient, mass_kg);
                (w[0].distance_m, w[0].gradient, watts as u32)
            })
            .collect()
    }
}

/// Haversine distance in metres between two GPS coordinates.
fn haversine(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f32 {
    const R: f64 = 6_371_000.0; // Earth radius in metres
    let dlat = (lat2 - lat1).to_radians();
    let dlng = (lng2 - lng1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlng / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    (R * c) as f32
}

fn parse_gpx(data: &[u8]) -> Result<Route> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let text = std::str::from_utf8(data).context("GPX file is not valid UTF-8")?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut name = String::from("GPX Route");
    let mut raw_pts: Vec<(f64, f64, f32)> = Vec::new(); // (lat, lng, elevation_m)
    let mut in_name = false;
    let mut in_ele = false;
    let mut current_lat: Option<f64> = None;
    let mut current_lng: Option<f64> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"name" => in_name = true,
                b"ele" => in_ele = true,
                b"trkpt" | b"rtept" | b"wpt" => {
                    let lat = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == b"lat")
                        .and_then(|a| {
                            std::str::from_utf8(a.value.as_ref())
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                        });
                    let lng = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == b"lon")
                        .and_then(|a| {
                            std::str::from_utf8(a.value.as_ref())
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                        });
                    current_lat = lat;
                    current_lng = lng;
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"name" => in_name = false,
                b"ele" => in_ele = false,
                b"trkpt" | b"rtept" | b"wpt" => {
                    if let (Some(lat), Some(lng)) = (current_lat, current_lng) {
                        let ele = raw_pts.last().map(|&(_, _, e)| e).unwrap_or(0.0);
                        raw_pts.push((lat, lng, ele));
                    }
                    current_lat = None;
                    current_lng = None;
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                let t = e.unescape().unwrap_or_default();
                if in_name && name == "GPX Route" {
                    name = t.trim().to_string();
                } else if in_ele {
                    if let (Some(lat), Some(lng)) = (current_lat, current_lng) {
                        if let Ok(ele) = t.trim().parse::<f32>() {
                            // Update elevation for the current pending point
                            raw_pts.push((lat, lng, ele));
                            current_lat = None;
                            current_lng = None;
                            in_ele = false;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("GPX parse error: {e}"),
            _ => {}
        }
    }

    anyhow::ensure!(!raw_pts.is_empty(), "GPX file contains no trackpoints");

    // Smooth elevation with a 5-point moving average to reduce GPS noise
    let smoothed_ele: Vec<f32> = {
        let n = raw_pts.len();
        (0..n)
            .map(|i| {
                let lo = i.saturating_sub(2);
                let hi = (i + 3).min(n);
                let sum: f32 = raw_pts[lo..hi].iter().map(|&(_, _, e)| e).sum();
                sum / (hi - lo) as f32
            })
            .collect()
    };

    // Build RoutePoints with cumulative distance and gradient
    let mut points: Vec<RoutePoint> = Vec::with_capacity(raw_pts.len());
    let mut cumulative_m = 0.0f32;
    let mut total_gain = 0.0f32;

    for (i, &(lat, lng, _)) in raw_pts.iter().enumerate() {
        let ele = smoothed_ele[i];
        let dist_to_next = if i + 1 < raw_pts.len() {
            haversine(lat, lng, raw_pts[i + 1].0, raw_pts[i + 1].1)
        } else {
            0.0
        };
        let ele_to_next = if i + 1 < raw_pts.len() {
            smoothed_ele[i + 1] - ele
        } else {
            0.0
        };

        let gradient = if dist_to_next > 0.1 {
            (ele_to_next / dist_to_next).clamp(-0.30, 0.30)
        } else {
            0.0
        };

        if ele_to_next > 0.0 {
            total_gain += ele_to_next;
        }

        points.push(RoutePoint {
            lat,
            lng,
            elevation_m: ele,
            distance_m: cumulative_m,
            gradient,
        });

        cumulative_m += dist_to_next;
    }

    let total_distance_m = cumulative_m;
    anyhow::ensure!(
        total_distance_m > 10.0,
        "GPX route is too short ({:.0} m)",
        total_distance_m
    );

    Ok(Route {
        name,
        points,
        total_distance_m,
        total_gain_m: total_gain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_distance() {
        // London to Paris is approximately 341 km
        let d = haversine(51.5074, -0.1278, 48.8566, 2.3522);
        assert!(
            (d - 341_500.0).abs() < 5000.0,
            "expected ~341 km, got {d:.0} m"
        );
    }

    #[test]
    fn power_model_flat_known_value() {
        // 10 m/s (36 km/h), 0% gradient, 80 kg combined — expect ~200–250 W
        let p = Route::power_at(10.0, 0.0, 80.0);
        assert!(
            (160.0..300.0).contains(&p),
            "expected ~200-250 W on flat at 10 m/s, got {p:.0} W"
        );
    }

    #[test]
    fn power_model_climb_higher_than_flat() {
        let flat = Route::power_at(10.0, 0.0, 80.0);
        let climb = Route::power_at(10.0, 0.08, 80.0);
        assert!(climb > flat, "climbing should require more power than flat");
    }

    // ── speed_from_power (SIM mode virtual speed) ────────────────────────────

    #[test]
    fn should_ride_around_34_kmh_at_200w_on_the_flat() {
        let v = Route::speed_from_power(200.0, 0.0, 75.0);
        assert!((9.0..10.0).contains(&v), "got {v} m/s");
    }

    #[test]
    fn should_slow_to_a_crawl_at_200w_on_8_percent() {
        let v = Route::speed_from_power(200.0, 0.08, 75.0);
        assert!((2.8..3.5).contains(&v), "got {v} m/s");
    }

    #[test]
    fn should_coast_downhill_at_zero_power() {
        // −5%: terminal coasting speed, well above zero
        let v = Route::speed_from_power(0.0, -0.05, 75.0);
        assert!((12.0..15.0).contains(&v), "got {v} m/s");
    }

    #[test]
    fn should_stand_still_at_zero_power_on_the_flat() {
        let v = Route::speed_from_power(0.0, 0.0, 75.0);
        assert!(v < 0.2, "got {v} m/s");
    }

    #[test]
    fn speed_from_power_inverts_power_at() {
        for &(watts, grade) in &[(150.0, 0.0), (250.0, 0.05), (100.0, -0.02)] {
            let v = Route::speed_from_power(watts, grade, 78.0);
            let p = Route::power_at(v, grade, 78.0);
            assert!((p - watts).abs() < 1.0, "P({v})={p}, expected {watts}");
        }
    }

    #[test]
    fn more_power_means_more_speed() {
        let slow = Route::speed_from_power(150.0, 0.04, 75.0);
        let fast = Route::speed_from_power(300.0, 0.04, 75.0);
        assert!(fast > slow);
    }

    #[test]
    fn parse_minimal_gpx() {
        let gpx = br#"<?xml version="1.0"?>
<gpx version="1.1">
  <trk>
    <name>Test Route</name>
    <trkseg>
      <trkpt lat="51.5" lon="-0.1"><ele>10</ele></trkpt>
      <trkpt lat="51.6" lon="-0.1"><ele>20</ele></trkpt>
      <trkpt lat="51.7" lon="-0.1"><ele>15</ele></trkpt>
    </trkseg>
  </trk>
</gpx>"#;
        let route = parse_gpx(gpx).unwrap();
        assert_eq!(route.name, "Test Route");
        assert_eq!(route.points.len(), 3);
        assert!(route.total_distance_m > 10_000.0, "should be > 10 km");
    }
}
