use anyhow::Result;
use std::path::{Path, PathBuf};

use super::session::{DataPoint, Session};

// FIT epoch: 1989-12-31 00:00:00 UTC = Unix timestamp 631065600
const FIT_EPOCH_OFFSET: i64 = 631_065_600;

fn unix_to_fit(unix_secs: i64) -> u32 {
    unix_secs.saturating_sub(FIT_EPOCH_OFFSET).max(0) as u32
}

fn crc16(data: &[u8]) -> u16 {
    const T: [u16; 16] = [
        0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401, 0xA001, 0x6C00, 0x7800,
        0xB401, 0x5000, 0x9C01, 0x8801, 0x4400,
    ];
    let mut crc: u16 = 0;
    for &b in data {
        let tmp = T[(crc & 0x0F) as usize];
        crc = (crc >> 4) & 0x0FFF;
        crc ^= tmp ^ T[(b & 0x0F) as usize];
        let tmp = T[(crc & 0x0F) as usize];
        crc = (crc >> 4) & 0x0FFF;
        crc ^= tmp ^ T[((b >> 4) & 0x0F) as usize];
    }
    crc
}

// Emit a little-endian definition message.
fn definition(buf: &mut Vec<u8>, local_type: u8, global_num: u16, fields: &[(u8, u8, u8)]) {
    buf.push(0x40 | local_type); // definition message header
    buf.push(0x00); // reserved
    buf.push(0x00); // architecture: little-endian
    buf.extend_from_slice(&global_num.to_le_bytes());
    buf.push(fields.len() as u8);
    for &(field_num, size, base_type) in fields {
        buf.push(field_num);
        buf.push(size);
        buf.push(base_type);
    }
}

fn avg_of<F: Fn(&super::session::DataPoint) -> Option<u32>>(
    session: &Session,
    f: F,
) -> Option<u32> {
    let vals: Vec<u32> = session.data_points.iter().filter_map(f).collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<u32>() / vals.len() as u32)
    }
}

/// Encode a session as a valid FIT activity file.
pub fn encode_session(session: &Session) -> Vec<u8> {
    let mut msgs: Vec<u8> = Vec::new();

    let start_ts = unix_to_fit(session.started_at.timestamp());
    let end_ts = session
        .ended_at
        .map(|t| unix_to_fit(t.timestamp()))
        .unwrap_or_else(|| start_ts + session.duration_secs() as u32);
    let elapsed_ms = session.duration_secs() as u32 * 1000;

    let avg_power = session
        .average_power()
        .map(|p| (p as u16).min(0xFFFE))
        .unwrap_or(0xFFFF);
    let np = session
        .normalised_power()
        .map(|p| (p as u16).min(0xFFFE))
        .unwrap_or(0xFFFF);
    let kj = session.kilojoules();
    let calories = kj as u16;
    let total_work_j = (kj * 1000.0).round() as u32;
    let avg_hr = avg_of(session, |p| p.heart_rate_bpm)
        .map(|v| v.min(0xFE) as u8)
        .unwrap_or(0xFF);
    let avg_cad = avg_of(session, |p| p.cadence_rpm)
        .map(|v| v.min(0xFE) as u8)
        .unwrap_or(0xFF);

    // ── file_id (local 0, global 0) ─────────────────────────────────────────
    definition(
        &mut msgs,
        0,
        0,
        &[
            (0, 1, 0x00), // type: enum
            (1, 2, 0x84), // manufacturer: uint16
            (2, 2, 0x84), // product: uint16
            (3, 4, 0x8C), // serial_number: uint32z
            (4, 4, 0x86), // time_created: uint32
        ],
    );
    msgs.push(0x00); // data: local type 0
    msgs.push(4); // type = activity
    msgs.extend_from_slice(&255u16.to_le_bytes()); // manufacturer = development
    msgs.extend_from_slice(&1u16.to_le_bytes()); // product
    msgs.extend_from_slice(&0u32.to_le_bytes()); // serial_number (unknown)
    msgs.extend_from_slice(&start_ts.to_le_bytes());

    // ── record (local 1, global 20) ─────────────────────────────────────────
    definition(
        &mut msgs,
        1,
        20,
        &[
            (253, 4, 0x86), // timestamp: uint32
            (7, 2, 0x84),   // power: uint16
            (3, 1, 0x02),   // heart_rate: uint8
            (4, 1, 0x02),   // cadence: uint8
        ],
    );
    for pt in &session.data_points {
        let ts = start_ts + pt.elapsed_secs;
        let power = pt
            .power_watts
            .map(|p| (p.min(4000)) as u16)
            .unwrap_or(0xFFFF);
        let hr = pt.heart_rate_bpm.map(|h| h.min(254) as u8).unwrap_or(0xFF);
        let cad = pt.cadence_rpm.map(|c| c.min(254) as u8).unwrap_or(0xFF);
        msgs.push(0x01); // data: local type 1
        msgs.extend_from_slice(&ts.to_le_bytes());
        msgs.extend_from_slice(&power.to_le_bytes());
        msgs.push(hr);
        msgs.push(cad);
    }

    // ── lap (local 2, global 19) ────────────────────────────────────────────
    definition(
        &mut msgs,
        2,
        19,
        &[
            (254, 2, 0x84), // message_index: uint16
            (253, 4, 0x86), // timestamp: uint32
            (0, 1, 0x00),   // event: enum
            (1, 1, 0x00),   // event_type: enum
            (2, 4, 0x86),   // start_time: uint32
            (7, 4, 0x86),   // total_elapsed_time: uint32 (ms)
            (8, 4, 0x86),   // total_timer_time: uint32 (ms)
            (11, 2, 0x84),  // total_calories: uint16
            (16, 1, 0x02),  // avg_heart_rate: uint8
            (18, 1, 0x02),  // avg_cadence: uint8
            (20, 2, 0x84),  // avg_power: uint16
            (32, 2, 0x84),  // normalized_power: uint16
            (41, 4, 0x86),  // total_work: uint32 (J)
            (24, 1, 0x00),  // lap_trigger: enum
            (25, 1, 0x00),  // sport: enum
        ],
    );
    msgs.push(0x02);
    msgs.extend_from_slice(&0u16.to_le_bytes()); // message_index
    msgs.extend_from_slice(&end_ts.to_le_bytes());
    msgs.push(9); // event = lap
    msgs.push(1); // event_type = stop_disable_all
    msgs.extend_from_slice(&start_ts.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&calories.to_le_bytes());
    msgs.push(avg_hr);
    msgs.push(avg_cad);
    msgs.extend_from_slice(&avg_power.to_le_bytes());
    msgs.extend_from_slice(&np.to_le_bytes());
    msgs.extend_from_slice(&total_work_j.to_le_bytes());
    msgs.push(0); // lap_trigger = manual
    msgs.push(2); // sport = cycling

    // ── session (local 3, global 18) ────────────────────────────────────────
    definition(
        &mut msgs,
        3,
        18,
        &[
            (254, 2, 0x84), // message_index: uint16
            (253, 4, 0x86), // timestamp: uint32
            (0, 1, 0x00),   // event: enum
            (1, 1, 0x00),   // event_type: enum
            (2, 4, 0x86),   // start_time: uint32
            (5, 1, 0x00),   // sport: enum
            (6, 1, 0x00),   // sub_sport: enum
            (7, 4, 0x86),   // total_elapsed_time: uint32 (ms)
            (8, 4, 0x86),   // total_timer_time: uint32 (ms)
            (11, 2, 0x84),  // total_calories: uint16
            (16, 1, 0x02),  // avg_heart_rate: uint8
            (18, 1, 0x02),  // avg_cadence: uint8
            (20, 2, 0x84),  // avg_power: uint16
            (34, 2, 0x84),  // normalized_power: uint16
            (25, 2, 0x84),  // first_lap_index: uint16
            (26, 2, 0x84),  // num_laps: uint16
            (28, 1, 0x00),  // trigger: enum
        ],
    );
    msgs.push(0x03);
    msgs.extend_from_slice(&0u16.to_le_bytes()); // message_index
    msgs.extend_from_slice(&end_ts.to_le_bytes());
    msgs.push(8); // event = session
    msgs.push(1); // event_type = stop_disable_all
    msgs.extend_from_slice(&start_ts.to_le_bytes());
    msgs.push(2); // sport = cycling
    msgs.push(6); // sub_sport = indoor_cycling
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&calories.to_le_bytes());
    msgs.push(avg_hr);
    msgs.push(avg_cad);
    msgs.extend_from_slice(&avg_power.to_le_bytes());
    msgs.extend_from_slice(&np.to_le_bytes());
    msgs.extend_from_slice(&0u16.to_le_bytes()); // first_lap_index = 0
    msgs.extend_from_slice(&1u16.to_le_bytes()); // num_laps = 1
    msgs.push(0); // trigger = activity_end

    // ── activity (local 4, global 34) ───────────────────────────────────────
    definition(
        &mut msgs,
        4,
        34,
        &[
            (253, 4, 0x86), // timestamp: uint32
            (0, 4, 0x86),   // total_timer_time: uint32 (ms)
            (5, 4, 0x86),   // local_timestamp: uint32
            (1, 2, 0x84),   // num_sessions: uint16
            (4, 1, 0x00),   // type: enum
            (2, 1, 0x00),   // event: enum
            (3, 1, 0x00),   // event_type: enum
        ],
    );
    msgs.push(0x04);
    msgs.extend_from_slice(&end_ts.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&end_ts.to_le_bytes()); // local_timestamp ≈ end_ts
    msgs.extend_from_slice(&1u16.to_le_bytes()); // num_sessions
    msgs.push(0); // type = manual
    msgs.push(26); // event = activity
    msgs.push(1); // event_type = stop_disable_all

    // ── Assemble file with header and CRCs ──────────────────────────────────
    let data_size = msgs.len() as u32;

    let mut header: Vec<u8> = Vec::new();
    header.push(14u8); // header size
    header.push(0x10u8); // protocol version 1.0
    header.extend_from_slice(&2100u16.to_le_bytes()); // profile version 21.00
    header.extend_from_slice(&data_size.to_le_bytes());
    header.extend_from_slice(b".FIT");
    // Header CRC covers the first 12 bytes
    let hdr_crc = crc16(&header);
    header.extend_from_slice(&hdr_crc.to_le_bytes());

    let file_crc = crc16(&msgs);

    let mut file = header;
    file.extend_from_slice(&msgs);
    file.extend_from_slice(&file_crc.to_le_bytes());
    file
}

/// Parse a FIT file from disk into a `Session`.
///
/// Extracts per-second records (power, HR, cadence, speed) and session timestamps.
/// Returns an error if the file is malformed, too large, or contains no activity data.
pub fn import_fit_file(path: &Path) -> Result<Session> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(
        bytes.len() <= 50 * 1024 * 1024,
        "FIT file too large ({} bytes — maximum 50 MB)",
        bytes.len()
    );

    let records =
        fitparser::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("FIT parse error: {e}"))?;

    let mut session_start: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut session_end: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut data_points: Vec<DataPoint> = Vec::new();

    for record in &records {
        use fitparser::profile::MesgNum;

        match record.kind() {
            // Session message carries authoritative start/end timestamps
            MesgNum::Session => {
                for field in record.fields() {
                    match field.name() {
                        "start_time" => {
                            if let fitparser::Value::Timestamp(ts) = field.value() {
                                session_start = Some(ts.with_timezone(&chrono::Utc));
                            }
                        }
                        "timestamp" => {
                            if let fitparser::Value::Timestamp(ts) = field.value() {
                                session_end = Some(ts.with_timezone(&chrono::Utc));
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Record messages are the per-second data points
            MesgNum::Record => {
                let mut ts: Option<chrono::DateTime<chrono::Utc>> = None;
                let mut power: Option<u32> = None;
                let mut hr: Option<u32> = None;
                let mut cad: Option<u32> = None;
                let mut speed_kmh: Option<f32> = None;
                let mut lat_deg: Option<f64> = None;
                let mut lng_deg: Option<f64> = None;

                for field in record.fields() {
                    match field.name() {
                        "timestamp" => {
                            if let fitparser::Value::Timestamp(t) = field.value() {
                                ts = Some(t.with_timezone(&chrono::Utc));
                            }
                        }
                        "power" => {
                            // Invalid FIT value: 0xFFFF
                            if let Some(v) = fit_u32(field.value()) {
                                if v < 0xFFFF {
                                    power = Some(v.min(3000));
                                }
                            }
                        }
                        "heart_rate" => {
                            // Invalid FIT value: 0xFF
                            if let Some(v) = fit_u32(field.value()) {
                                if v < 0xFF {
                                    hr = Some(v.min(250));
                                }
                            }
                        }
                        "cadence" => {
                            // Invalid FIT value: 0xFF
                            if let Some(v) = fit_u32(field.value()) {
                                if v < 0xFF {
                                    cad = Some(v.min(250));
                                }
                            }
                        }
                        "speed" => {
                            // FIT speed: uint16 in units of mm/s
                            if let Some(v) = fit_u32(field.value()) {
                                if v < 0xFFFF {
                                    speed_kmh = Some(v as f32 / 1000.0 * 3.6);
                                }
                            }
                        }
                        "enhanced_speed" => {
                            // enhanced_speed: float64 in m/s (overrides speed if present)
                            if let fitparser::Value::Float64(v) = field.value() {
                                if v.is_finite() && *v >= 0.0 {
                                    speed_kmh = Some(*v as f32 * 3.6);
                                }
                            }
                        }
                        "position_lat" => {
                            // FIT GPS: SInt32 semicircles; i32::MAX is the invalid sentinel
                            if let fitparser::Value::SInt32(v) = field.value() {
                                if *v != i32::MAX {
                                    lat_deg = Some(*v as f64 * (180.0 / 2_147_483_648.0));
                                }
                            }
                        }
                        "position_long" => {
                            if let fitparser::Value::SInt32(v) = field.value() {
                                if *v != i32::MAX {
                                    lng_deg = Some(*v as f64 * (180.0 / 2_147_483_648.0));
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if let Some(ts) = ts {
                    // session_start might not be set yet if Session message comes later —
                    // track the earliest record timestamp as a fallback
                    if session_start.is_none() {
                        session_start = Some(ts);
                    }
                    session_end = Some(ts);

                    let start = session_start
                        .expect("invariant: session_start was just set to Some(ts) above");
                    let elapsed = (ts - start).num_seconds().max(0) as u32;
                    data_points.push(DataPoint {
                        elapsed_secs: elapsed,
                        power_watts: power,
                        heart_rate_bpm: hr,
                        cadence_rpm: cad,
                        speed_kmh,
                        lat: lat_deg,
                        lng: lng_deg,
                    });
                }
            }

            _ => {}
        }
    }

    let started_at =
        session_start.ok_or_else(|| anyhow::anyhow!("FIT file contains no session start time"))?;

    anyhow::ensure!(
        !data_points.is_empty(),
        "FIT file contains no activity records"
    );

    // Ensure chronological order and collapse duplicate timestamps (keep last value)
    data_points.sort_by_key(|dp| dp.elapsed_secs);
    data_points.dedup_by_key(|dp| dp.elapsed_secs);

    let gps_count = data_points.iter().filter(|dp| dp.lat.is_some()).count();
    tracing::info!(
        total_records = data_points.len(),
        gps_records = gps_count,
        "FIT file imported"
    );

    Ok(Session {
        id: 0,
        workout_id: None,
        started_at,
        ended_at: session_end,
        data_points,
        rpe: None,
    })
}

/// Extract a u32 from common FIT numeric value variants.
fn fit_u32(value: &fitparser::Value) -> Option<u32> {
    match value {
        fitparser::Value::UInt8(v) => Some(*v as u32),
        fitparser::Value::UInt16(v) => Some(*v as u32),
        fitparser::Value::UInt32(v) => Some(*v),
        fitparser::Value::SInt8(v) if *v >= 0 => Some(*v as u32),
        fitparser::Value::SInt16(v) if *v >= 0 => Some(*v as u32),
        fitparser::Value::SInt32(v) if *v >= 0 => Some(*v as u32),
        _ => None,
    }
}

/// Write the FIT file to `~/.local/share/cycle/exports/` and return the path.
pub fn export_to_xdg_path(session: &Session) -> Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".local/share")
        });
    let exports_dir = base.join("cycle").join("exports");
    std::fs::create_dir_all(&exports_dir)?;
    let ts = session.started_at.format("%Y-%m-%d_%H%M%S").to_string();
    let path = exports_dir.join(format!("workout_{}.fit", ts));
    std::fs::write(&path, encode_session(session))?;
    Ok(path)
}
