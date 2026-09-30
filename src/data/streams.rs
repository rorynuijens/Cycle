use serde_json::Value;

/// The longest activity [`ActivityStreams::watts_per_second`] will lay out.
///
/// The time stream is third-party data: one corrupt sample reading `u32::MAX`
/// would otherwise ask for a sixteen-gigabyte vector. Two days covers any ride
/// a person could do in one sitting.
pub const MAX_RESAMPLED_SECS: u32 = 48 * 3600;

/// Ceiling on any single power sample. A glitching power meter can report
/// 65 535 W; kept as-is it becomes an all-time 5 s best that squashes the
/// rest of the power curve flat (CLAUDE.md §5.1).
const MAX_PLAUSIBLE_WATTS: u32 = 3000;

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
                "watts" => {
                    s.watts = json_u32_vec(data)
                        .into_iter()
                        .map(|w| w.min(MAX_PLAUSIBLE_WATTS))
                        .collect()
                }
                "velocity_smooth" => s.velocity_ms = json_f32_vec(data),
                // The streams endpoint splits the track into `data` (latitude)
                // and `data2` (longitude); the map endpoint sends pairs.
                "latlng" => {
                    s.latlng = match item.get("data2") {
                        Some(lngs) if lngs.is_array() => json_split_latlng_vec(data, lngs),
                        _ => json_latlng_vec(data),
                    }
                }
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

    /// Power laid out one value per elapsed second, for rolling-window bests.
    ///
    /// Outdoor recordings skip seconds — auto-pause, a stop at the lights — so
    /// the samples cannot be windowed by index: two efforts either side of a
    /// café stop would merge into one. Missing seconds become 0 W, which can
    /// only ever lower a best, never invent one. Samples whose time does not
    /// move forward are dropped, and nothing at or past
    /// [`MAX_RESAMPLED_SECS`] is laid out. Without a time stream the samples
    /// are taken as 1 Hz.
    pub fn watts_per_second(&self) -> Vec<u32> {
        if self.time_s.is_empty() {
            return self.watts.clone();
        }
        // `out` is indexed by seconds since the first sample.
        let first = self.time_s[0];
        let mut out: Vec<u32> = Vec::with_capacity(self.watts.len());
        let mut last: Option<u32> = None;
        for (&t, &w) in self.time_s.iter().zip(&self.watts) {
            if t >= MAX_RESAMPLED_SECS {
                break;
            }
            if last.is_some_and(|prev| t <= prev) {
                continue;
            }
            out.resize((t - first) as usize, 0);
            out.push(w);
            last = Some(t);
        }
        out
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

/// Zip parallel latitude and longitude arrays, dropping any point where either
/// half is missing.
fn json_split_latlng_vec(lats: &Value, lngs: &Value) -> Vec<(f64, f64)> {
    let (Some(lats), Some(lngs)) = (lats.as_array(), lngs.as_array()) else {
        return Vec::new();
    };
    lats.iter()
        .zip(lngs)
        .filter_map(|(lat, lng)| Some((lat.as_f64()?, lng.as_f64()?)))
        .collect()
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

    // ── Intervals.icu's real shapes ──────────────────────────────────────────

    #[test]
    fn should_parse_latlng_split_across_data_and_data2() {
        // The streams endpoint sends latitudes in `data` and longitudes in
        // `data2`, not pairs. Shape copied from a real 2026-08-23 ride.
        let json = r#"[
            {"type":"latlng","name":null,"data":[40.94468,40.944687,40.94459],
             "data2":[29.143288,29.143269,29.143133],"valueTypeIsArray":false}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(
            s.latlng,
            vec![
                (40.94468, 29.143288),
                (40.944687, 29.143269),
                (40.94459, 29.143133)
            ]
        );
    }

    #[test]
    fn should_drop_split_latlng_points_missing_either_half() {
        // A GPS dropout arrives as nulls; a short data2 must not index past its end.
        let json = r#"[
            {"type":"latlng","data":[40.0,null,41.0,42.0],"data2":[29.0,30.0,null]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.latlng, vec![(40.0, 29.0)]);
    }

    #[test]
    fn should_still_parse_latlng_pairs_from_the_map_endpoint() {
        let json = r#"[{"type":"latlng","data":[[40.0,29.0],[40.1,29.1]]}]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.latlng, vec![(40.0, 29.0), (40.1, 29.1)]);
    }

    #[test]
    fn should_read_latlng_pairs_when_data2_is_null() {
        // Every stream carries a `data2` key; on most it is null. Null must
        // not route the pairs through the split reader and lose the track.
        let json = r#"[{"type":"latlng","data":[[40.0,29.0],[40.1,29.1]],"data2":null}]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.latlng, vec![(40.0, 29.0), (40.1, 29.1)]);
    }

    #[test]
    fn should_read_every_stream_type_the_app_asks_for() {
        let json = r#"[
            {"type":"time","data":[0,1]},
            {"type":"watts","data":[150,160]},
            {"type":"heartrate","data":[120,121]},
            {"type":"cadence","data":[85,86]},
            {"type":"distance","data":[0.0,8.5]},
            {"type":"altitude","data":[12.0,12.5]},
            {"type":"velocity_smooth","data":[8.25,8.5]},
            {"type":"latlng","data":[40.0,40.1],"data2":[29.0,29.1]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.time_s, vec![0, 1]);
        assert_eq!(s.watts, vec![150, 160]);
        assert_eq!(s.heartrate, vec![120, 121]);
        assert_eq!(s.cadence, vec![85, 86]);
        assert_eq!(s.distance_m, vec![0.0, 8.5]);
        assert_eq!(s.altitude_m, vec![12.0, 12.5]);
        assert_eq!(s.velocity_ms, vec![8.25, 8.5]);
        assert_eq!(s.latlng, vec![(40.0, 29.0), (40.1, 29.1)]);
    }

    // ── watts_per_second ─────────────────────────────────────────────────────

    #[test]
    fn should_fill_an_auto_pause_gap_with_zero_watts() {
        // Real outdoor rides skip seconds: this time stream jumps 1 → 12.
        let json = r#"[
            {"type":"time","data":[0,1,12,13]},
            {"type":"watts","data":[100,200,300,400]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        let per_sec = s.watts_per_second();
        assert_eq!(per_sec.len(), 14);
        assert_eq!(&per_sec[..2], &[100, 200]);
        assert!(per_sec[2..12].iter().all(|&w| w == 0));
        assert_eq!(&per_sec[12..], &[300, 400]);
    }

    #[test]
    fn should_drop_samples_whose_time_does_not_move_forward() {
        let json = r#"[
            {"type":"time","data":[0,1,1,0,2]},
            {"type":"watts","data":[10,20,30,40,50]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts_per_second(), vec![10, 20, 50]);
    }

    #[test]
    fn should_not_allocate_for_a_time_stream_that_leaps_to_u32_max() {
        let json = r#"[
            {"type":"time","data":[0,1,4294967295]},
            {"type":"watts","data":[10,20,30]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts_per_second(), vec![10, 20]);
    }

    #[test]
    fn should_keep_the_last_second_before_the_resample_cap() {
        let last = MAX_RESAMPLED_SECS - 1;
        let json = format!(
            r#"[{{"type":"time","data":[0,{last},{MAX_RESAMPLED_SECS}]}},
                {{"type":"watts","data":[10,20,30]}}]"#
        );
        let s = ActivityStreams::from_json(&json).unwrap();
        let per_sec = s.watts_per_second();
        assert_eq!(per_sec.len(), MAX_RESAMPLED_SECS as usize);
        assert_eq!(per_sec[last as usize], 20);
    }

    #[test]
    fn should_clamp_an_implausible_power_spike() {
        let json = r#"[
            {"type":"time","data":[0,1,2]},
            {"type":"watts","data":[3000,3001,65535]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts, vec![3000, 3000, 3000]);
        assert_eq!(s.watts_per_second(), vec![3000, 3000, 3000]);
    }

    #[test]
    fn should_treat_watts_without_a_time_stream_as_one_hertz() {
        let json = r#"[{"type":"watts","data":[5,6,7]}]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts_per_second(), vec![5, 6, 7]);
    }

    #[test]
    fn should_ignore_watts_beyond_the_end_of_the_time_stream() {
        let json = r#"[
            {"type":"time","data":[0,1]},
            {"type":"watts","data":[5,6,7,8]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts_per_second(), vec![5, 6]);
    }

    #[test]
    fn should_index_from_the_first_sample_not_from_zero() {
        let json = r#"[
            {"type":"time","data":[100,101,103]},
            {"type":"watts","data":[10,20,30]}
        ]"#;
        let s = ActivityStreams::from_json(json).unwrap();
        assert_eq!(s.watts_per_second(), vec![10, 20, 0, 30]);
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

    mod search {
        use super::*;
        use proptest::prelude::*;

        /// Times a real stream carries, plus the ones a corrupt one might.
        fn any_time() -> impl Strategy<Value = u32> {
            prop_oneof![
                6 => 0u32..20_000,
                1 => Just(0u32),
                1 => Just(MAX_RESAMPLED_SECS - 1),
                1 => Just(MAX_RESAMPLED_SECS),
                1 => Just(u32::MAX),
            ]
        }

        proptest! {
            /// Resampling never panics, never grows past the cap, never lets
            /// a sample exceed the plausibility ceiling, and never invents
            /// power: every watt out was a watt in.
            #[test]
            fn should_only_ever_redistribute_power(
                times in proptest::collection::vec(any_time(), 0..200),
                watts in proptest::collection::vec(any::<u32>(), 0..200),
            ) {
                let json = serde_json::json!([
                    {"type": "time", "data": times},
                    {"type": "watts", "data": watts},
                ])
                .to_string();
                let s = ActivityStreams::from_json(&json).unwrap();
                let per_sec = s.watts_per_second();
                prop_assert!(per_sec.len() <= MAX_RESAMPLED_SECS as usize);
                prop_assert!(per_sec.iter().all(|&w| w <= MAX_PLAUSIBLE_WATTS));
                let sum_in: u64 = s.watts.iter().map(|&w| w as u64).sum();
                let sum_out: u64 = per_sec.iter().map(|&w| w as u64).sum();
                prop_assert!(sum_out <= sum_in);
            }

            /// Any JSON value in any stream slot parses without panicking.
            #[test]
            fn should_survive_any_json_in_a_stream(
                kind in prop_oneof![
                    Just("time"), Just("watts"), Just("latlng"), Just("heartrate"),
                    Just("distance"),
                ],
                data in prop_oneof![
                    Just(serde_json::json!(null)),
                    Just(serde_json::json!("x")),
                    Just(serde_json::json!([null, -1, 1e308, "x", [1], [1, 2, 3]])),
                    Just(serde_json::json!([[null, 1.0], [1.0]])),
                ],
                data2 in prop_oneof![
                    Just(serde_json::json!(null)),
                    Just(serde_json::json!([])),
                    Just(serde_json::json!([1.0, null, "x"])),
                ],
            ) {
                let json = serde_json::json!([{"type": kind, "data": data, "data2": data2}])
                    .to_string();
                let s = ActivityStreams::from_json(&json).unwrap();
                let _ = s.watts_per_second();
            }
        }
    }
}
