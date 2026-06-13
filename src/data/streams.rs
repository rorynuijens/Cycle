use serde_json::Value;

/// Per-second (or per-point) activity streams fetched from the Intervals.icu streams API.
#[derive(Debug, Default, Clone)]
pub struct ActivityStreams {
    pub time_s: Vec<u32>,
    pub distance_m: Vec<f32>,
    pub altitude_m: Vec<f32>,
    pub heartrate: Vec<u32>,
    pub cadence: Vec<u32>,
    pub watts: Vec<u32>,
    pub velocity_ms: Vec<f32>,
    pub latlng: Vec<(f64, f64)>,
}

impl ActivityStreams {
    /// Parse from the Intervals.icu array-of-objects streams format:
    /// `[{"type": "time", "data": [...]}, {"type": "latlng", "data": [[lat, lng], ...]}, ...]`
    ///
    /// Returns `None` only if `json` is not valid JSON at all; an empty struct is returned for
    /// valid JSON with no recognised stream types.
    pub fn from_json(json: &str) -> Option<Self> {
        let arr: Vec<Value> = serde_json::from_str(json).ok()?;
        let mut s = ActivityStreams::default();

        for item in &arr {
            let Some(stream_type) = item.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(data) = item.get("data") else {
                continue;
            };

            match stream_type {
                "time" => s.time_s = json_u32_vec(data),
                "distance" => s.distance_m = json_f32_vec(data),
                "altitude" => s.altitude_m = json_f32_vec(data),
                "heartrate" => s.heartrate = json_u32_vec(data),
                "cadence" => s.cadence = json_u32_vec(data),
                "watts" => s.watts = json_u32_vec(data),
                "velocity_smooth" => s.velocity_ms = json_f32_vec(data),
                "latlng" => s.latlng = json_latlng_vec(data),
                _ => {}
            }
        }

        Some(s)
    }

    pub fn has_gps(&self) -> bool {
        self.latlng.len() >= 2
    }

    pub fn has_altitude(&self) -> bool {
        self.altitude_m.len() >= 2
    }

    pub fn has_hr(&self) -> bool {
        !self.heartrate.is_empty()
    }

    pub fn has_power(&self) -> bool {
        !self.watts.is_empty()
    }

    pub fn has_velocity(&self) -> bool {
        !self.velocity_ms.is_empty()
    }

    /// (x, altitude_m) pairs for elevation profile drawing.
    /// x is distance in metres when available, otherwise elapsed time in seconds.
    pub fn elevation_pairs(&self) -> Vec<(f32, f32)> {
        if self.altitude_m.is_empty() {
            return Vec::new();
        }
        if !self.distance_m.is_empty() {
            self.distance_m
                .iter()
                .zip(self.altitude_m.iter())
                .map(|(&d, &a)| (d, a))
                .collect()
        } else {
            self.time_s
                .iter()
                .zip(self.altitude_m.iter())
                .map(|(&t, &a)| (t as f32, a))
                .collect()
        }
    }

    /// Reduce `data` to at most `max_points` evenly-spaced samples.
    pub fn downsample<T: Copy>(data: &[T], max_points: usize) -> Vec<T> {
        if data.len() <= max_points || max_points == 0 {
            return data.to_vec();
        }
        let step = data.len() as f64 / max_points as f64;
        (0..max_points)
            .map(|i| data[(i as f64 * step) as usize])
            .collect()
    }

    /// Minimum elapsed seconds to cover `min_distance_m` using the cumulative distance stream.
    ///
    /// Uses a two-pointer sliding window: for each `hi`, advances `lo` as far right as
    /// possible while the window still covers `min_distance_m`, giving the shortest
    /// elapsed time for that effort distance.
    ///
    /// Returns `None` when either stream is absent or the total distance is less than
    /// `min_distance_m`.
    pub fn best_time_for_distance(&self, min_distance_m: f32) -> Option<u32> {
        let n = self.time_s.len().min(self.distance_m.len());
        if n < 2 {
            return None;
        }
        if *self.distance_m.get(n - 1).unwrap_or(&0.0) < min_distance_m {
            return None;
        }
        let mut best: Option<u32> = None;
        let mut lo = 0usize;
        for hi in 0..n {
            while lo + 1 < hi && self.distance_m[hi] - self.distance_m[lo + 1] >= min_distance_m {
                lo += 1;
            }
            if self.distance_m[hi] - self.distance_m[lo] >= min_distance_m {
                let elapsed = self.time_s[hi].saturating_sub(self.time_s[lo]);
                if elapsed > 0 {
                    best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
                }
            }
        }
        best
    }
}

fn json_u32_vec(val: &Value) -> Vec<u32> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_f64().unwrap_or(0.0).max(0.0) as u32)
                .collect()
        })
        .unwrap_or_default()
}

fn json_f32_vec(val: &Value) -> Vec<f32> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                .collect()
        })
        .unwrap_or_default()
}

fn json_latlng_vec(val: &Value) -> Vec<(f64, f64)> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let pair = v.as_array()?;
                    if pair.len() < 2 {
                        return None;
                    }
                    Some((pair[0].as_f64()?, pair[1].as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_time_and_latlng_streams() {
        let json = r#"[
            {"type":"time","data":[0,1,2]},
            {"type":"latlng","data":[[47.1,8.2],[47.101,8.201],[47.102,8.202]]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.time_s, vec![0, 1, 2]);
        assert!(s.has_gps());
        assert_eq!(s.latlng[0], (47.1, 8.2));
    }

    #[test]
    fn should_return_none_for_invalid_json() {
        assert!(ActivityStreams::from_json("not json").is_none());
    }

    #[test]
    fn should_downsample_to_max_points() {
        let data: Vec<u32> = (0..1000).collect();
        let ds = ActivityStreams::downsample(&data, 100);
        assert_eq!(ds.len(), 100);
        assert_eq!(ds[0], 0);
    }

    #[test]
    fn should_find_best_time_for_distance() {
        // 250 m/min constant pace: 1000 m takes 240 s
        let json = r#"[
            {"type":"time","data":[0,60,120,180,240]},
            {"type":"distance","data":[0.0,250.0,500.0,750.0,1000.0]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.best_time_for_distance(1000.0), Some(240));
        // Tightest 500 m window: indices 0–2, time = 120 s
        assert_eq!(s.best_time_for_distance(500.0), Some(120));
        // Not enough distance for 1500 m
        assert_eq!(s.best_time_for_distance(1500.0), None);
    }

    #[test]
    fn should_use_distance_as_elevation_x_axis_when_available() {
        let json = r#"[
            {"type":"distance","data":[0.0,100.0,200.0]},
            {"type":"altitude","data":[150.0,155.0,160.0]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        let pairs = s.elevation_pairs();
        assert_eq!(pairs[1], (100.0, 155.0));
    }
}
