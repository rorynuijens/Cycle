use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::athlete::AthleteProfile;
use super::session::{DataPoint, Session};

// FIT epoch: 1989-12-31 00:00:00 UTC = Unix timestamp 631065600
const FIT_EPOCH_OFFSET: i64 = 631_065_600;

// ── Recording device identity ───────────────────────────────────────────────
//
// Exports claim to come from the rider's own Edge 840 rather than from this
// app. Garmin Connect reads the training-load fields (session 24, 137, 168)
// only from a file that identifies as a recognised recording device: exporting
// under `manufacturer: 255` (development) was tried on 2026-08-03 and Connect
// discarded the values outright, showing no Training Effect on the activity and
// leaving acute load at 1.
//
// This misrepresents which device recorded the ride, and is very likely against
// Garmin Connect's terms. It is set at the rider's explicit instruction, for
// their own rides in their own account — do not change it back, or extend the
// same trick anywhere else, without asking them.
const MANUFACTURER_ID: u16 = 1; // garmin
const PRODUCT_ID: u16 = 4062; // Edge 840
const DEVICE_SERIAL: u32 = 3_444_313_785;
/// Firmware version, as FIT stores it: hundredths, so 2618 is 26.18.
const SOFTWARE_VERSION: u16 = 2618;

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

/// Degrees → semicircles, the angular unit FIT stores positions in.
/// `None` (and any non-finite value) becomes the invalid sentinel `i32::MAX`.
fn semicircles(degrees: Option<f64>) -> i32 {
    match degrees {
        Some(d) if d.is_finite() => (d * (2_147_483_648.0 / 180.0)) as i32,
        _ => i32::MAX,
    }
}

/// A filename-safe name for an exported activity, e.g. `Alpe_d_Huez-2026-08-01-1930.fit`.
pub fn suggested_filename(session: &Session, title: &str) -> String {
    let safe: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let safe = safe.trim_matches('_');
    let stem = if safe.is_empty() { "Ride" } else { safe };
    format!(
        "{stem}-{}.fit",
        session
            .started_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d-%H%M")
    )
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
///
/// The athlete profile supplies the numbers the training-load estimate needs
/// (FTP, max and resting heart rate). Garmin Connect will not compute training
/// load for a file it did not record — it reads the finished values out of the
/// session message — so the export carries them; see [`crate::training::load`].
pub fn encode_session(session: &Session, athlete: &AthleteProfile) -> Vec<u8> {
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
    let max_power = session
        .max_power()
        .map(|p| (p.min(0xFFFE)) as u16)
        .unwrap_or(0xFFFF);
    let max_hr = session.max_hr().map(|h| h.min(254) as u8).unwrap_or(0xFF);
    let total_ascent_m = session
        .elevation_gain_m()
        .map(|g| g.clamp(0.0, 65534.0) as u16)
        .unwrap_or(0);
    // A ride carrying GPS positions followed a course, so it is reported as a
    // virtual activity — that is what makes services draw it on a map instead of
    // filing it as a stationary indoor session.
    let has_position = session.data_points.iter().any(|p| p.lat.is_some());
    let has_hr = session
        .data_points
        .iter()
        .any(|p| p.heart_rate_bpm.is_some());

    let total_descent_m = session
        .elevation_loss_m()
        .map(|d| d.clamp(0.0, 65534.0) as u16)
        .unwrap_or(0);
    let max_speed_mms = session
        .max_speed_kmh()
        .map(|kmh| (kmh / 3.6 * 1000.0).round().clamp(0.0, 65534.0) as u16)
        .unwrap_or(0xFFFF);
    // Crank revolutions, summing cadence over the ride.
    let total_cycles = session
        .data_points
        .iter()
        .filter_map(|p| p.cadence_rpm)
        .map(|rpm| rpm as f64 / 60.0)
        .sum::<f64>()
        .round() as u32;

    // The FTP the ride was actually ridden against, which is what makes TSS and
    // intensity factor mean anything to a service reading them later.
    let ftp = session.ftp_watts.unwrap_or(athlete.ftp_watts);
    let tss = session
        .tss(ftp)
        .map(|t| (t * 10.0).round().clamp(0.0, 65534.0) as u16)
        .unwrap_or(0xFFFF);
    let intensity_factor = match (session.normalised_power(), ftp) {
        (Some(np), f) if f > 0 => (np / f as f32 * 1000.0).round().clamp(0.0, 65534.0) as u16,
        _ => 0xFFFF,
    };
    let load = crate::training::load::estimate(session, athlete);
    let training_load_peak = load
        .map(|l| (l.load * 65536.0).round().clamp(0.0, i32::MAX as f32 - 1.0) as i32)
        .unwrap_or(i32::MAX);
    let aerobic_te = load
        .map(|l| (l.aerobic_te * 10.0).round().clamp(0.0, 254.0) as u8)
        .unwrap_or(0xFF);
    let anaerobic_te = load
        .map(|l| (l.anaerobic_te * 10.0).round().clamp(0.0, 254.0) as u8)
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
    msgs.extend_from_slice(&MANUFACTURER_ID.to_le_bytes());
    msgs.extend_from_slice(&PRODUCT_ID.to_le_bytes());
    msgs.extend_from_slice(&DEVICE_SERIAL.to_le_bytes());
    msgs.extend_from_slice(&start_ts.to_le_bytes());

    // ── file_creator (local 7, global 49) ───────────────────────────────────
    // Every device-written file carries one; its absence is one more signal
    // that a file was produced by something other than a head unit.
    definition(
        &mut msgs,
        7,
        49,
        &[
            (0, 2, 0x84), // software_version: uint16
            (1, 2, 0x84), // hardware_version: uint16
        ],
    );
    msgs.push(0x07);
    msgs.extend_from_slice(&SOFTWARE_VERSION.to_le_bytes());
    msgs.extend_from_slice(&0xFFFFu16.to_le_bytes()); // hardware_version unknown

    // ── device_info (local 5, global 23) ────────────────────────────────────
    // A device-recorded file says what recorded it and which sensors were on.
    // Without this an activity reads as a manual entry, and manual entries are
    // excluded from training status.
    definition(
        &mut msgs,
        5,
        23,
        &[
            (253, 4, 0x86), // timestamp: uint32
            (0, 1, 0x02),   // device_index: uint8
            (1, 1, 0x02),   // device_type: uint8
            (2, 2, 0x84),   // manufacturer: uint16
            (3, 4, 0x8C),   // serial_number: uint32z
            (4, 2, 0x84),   // product: uint16
            (5, 2, 0x84),   // software_version: uint16 (100/version)
            (25, 1, 0x00),  // source_type: enum
        ],
    );
    msgs.push(0x05);
    msgs.extend_from_slice(&start_ts.to_le_bytes());
    msgs.push(0); // device_index = creator
    msgs.push(0xFF); // device_type: not an external sensor
    msgs.extend_from_slice(&MANUFACTURER_ID.to_le_bytes());
    msgs.extend_from_slice(&DEVICE_SERIAL.to_le_bytes());
    msgs.extend_from_slice(&PRODUCT_ID.to_le_bytes());
    msgs.extend_from_slice(&SOFTWARE_VERSION.to_le_bytes());
    msgs.push(5); // source_type = local

    if has_hr {
        msgs.push(0x05);
        msgs.extend_from_slice(&start_ts.to_le_bytes());
        msgs.push(1); // device_index
        msgs.push(120); // device_type = heart_rate
                        // The strap is a third-party ANT+ sensor, so it stays anonymous — only
                        // the recording device carries an identity.
        msgs.extend_from_slice(&0xFFFFu16.to_le_bytes()); // manufacturer unknown
        msgs.extend_from_slice(&0u32.to_le_bytes()); // serial_number unknown
        msgs.extend_from_slice(&0xFFFFu16.to_le_bytes()); // product unknown
        msgs.extend_from_slice(&0xFFFFu16.to_le_bytes()); // software_version unknown
        msgs.push(1); // source_type = antplus
    }

    // ── event (local 6, global 21) ──────────────────────────────────────────
    // Timer start and stop bracket the records. Garmin Connect uses these to
    // tell a recording apart from a summary someone typed in.
    definition(
        &mut msgs,
        6,
        21,
        &[
            (253, 4, 0x86), // timestamp: uint32
            (0, 1, 0x00),   // event: enum
            (1, 1, 0x00),   // event_type: enum
            (4, 1, 0x02),   // event_group: uint8
        ],
    );
    msgs.push(0x06);
    msgs.extend_from_slice(&start_ts.to_le_bytes());
    msgs.push(0); // event = timer
    msgs.push(0); // event_type = start
    msgs.push(0); // event_group

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
            (0, 4, 0x85),   // position_lat: sint32 (semicircles)
            (1, 4, 0x85),   // position_long: sint32 (semicircles)
            (2, 2, 0x84),   // altitude: uint16 (5/m, offset 500 m)
            (5, 4, 0x86),   // distance: uint32 (cm)
            (6, 2, 0x84),   // speed: uint16 (mm/s)
        ],
    );
    let mut distance_m = 0.0f32;
    for pt in &session.data_points {
        let ts = start_ts + pt.elapsed_secs;
        let power = pt
            .power_watts
            .map(|p| (p.min(4000)) as u16)
            .unwrap_or(0xFFFF);
        let hr = pt.heart_rate_bpm.map(|h| h.min(254) as u8).unwrap_or(0xFF);
        let cad = pt.cadence_rpm.map(|c| c.min(254) as u8).unwrap_or(0xFF);
        // Data points are one second apart, so speed integrates straight to distance.
        if let Some(kmh) = pt.speed_kmh {
            distance_m += kmh / 3.6;
        }
        let alt = pt
            .altitude_m
            .map(|a| ((a + 500.0) * 5.0).round().clamp(0.0, 65534.0) as u16)
            .unwrap_or(0xFFFF);
        let speed_mms = pt
            .speed_kmh
            .map(|kmh| (kmh / 3.6 * 1000.0).round().clamp(0.0, 65534.0) as u16)
            .unwrap_or(0xFFFF);
        msgs.push(0x01); // data: local type 1
        msgs.extend_from_slice(&ts.to_le_bytes());
        msgs.extend_from_slice(&power.to_le_bytes());
        msgs.push(hr);
        msgs.push(cad);
        msgs.extend_from_slice(&semicircles(pt.lat).to_le_bytes());
        msgs.extend_from_slice(&semicircles(pt.lng).to_le_bytes());
        msgs.extend_from_slice(&alt.to_le_bytes());
        msgs.extend_from_slice(&((distance_m * 100.0) as u32).to_le_bytes());
        msgs.extend_from_slice(&speed_mms.to_le_bytes());
    }
    let total_distance_cm = (distance_m * 100.0) as u32;
    let avg_speed_mms = if session.duration_secs() > 0 {
        (distance_m / session.duration_secs() as f32 * 1000.0)
            .round()
            .clamp(0.0, 65534.0) as u16
    } else {
        0xFFFF
    };

    // Timer stop closes the recording the start event opened.
    msgs.push(0x06);
    msgs.extend_from_slice(&end_ts.to_le_bytes());
    msgs.push(0); // event = timer
    msgs.push(4); // event_type = stop_all
    msgs.push(0); // event_group

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
            (9, 4, 0x86),   // total_distance: uint32 (cm)
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
    msgs.extend_from_slice(&total_distance_cm.to_le_bytes());
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
            (9, 4, 0x86),   // total_distance: uint32 (cm)
            (22, 2, 0x84),  // total_ascent: uint16 (m)
            (23, 2, 0x84),  // total_descent: uint16 (m)
            (21, 2, 0x84),  // max_power: uint16
            (17, 1, 0x02),  // max_heart_rate: uint8
            (10, 4, 0x86),  // total_cycles: uint32 (crank revolutions)
            (14, 2, 0x84),  // avg_speed: uint16 (mm/s)
            (15, 2, 0x84),  // max_speed: uint16 (mm/s)
            (45, 2, 0x84),  // threshold_power: uint16 (W)
            (35, 2, 0x84),  // training_stress_score: uint16 (10/TSS)
            (36, 2, 0x84),  // intensity_factor: uint16 (1000/IF)
            (24, 1, 0x02),  // total_training_effect: uint8 (10/TE)
            (137, 1, 0x02), // total_anaerobic_training_effect: uint8 (10/TE)
            (168, 4, 0x85), // training_load_peak: sint32 (65536/load)
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
    msgs.push(if has_position {
        58 // sub_sport = virtual_activity
    } else {
        6 // sub_sport = indoor_cycling
    });
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&calories.to_le_bytes());
    msgs.push(avg_hr);
    msgs.push(avg_cad);
    msgs.extend_from_slice(&avg_power.to_le_bytes());
    msgs.extend_from_slice(&np.to_le_bytes());
    msgs.extend_from_slice(&0u16.to_le_bytes()); // first_lap_index = 0
    msgs.extend_from_slice(&1u16.to_le_bytes()); // num_laps = 1
    msgs.extend_from_slice(&total_distance_cm.to_le_bytes());
    msgs.extend_from_slice(&total_ascent_m.to_le_bytes());
    msgs.extend_from_slice(&total_descent_m.to_le_bytes());
    msgs.extend_from_slice(&max_power.to_le_bytes());
    msgs.push(max_hr);
    msgs.extend_from_slice(&total_cycles.to_le_bytes());
    msgs.extend_from_slice(&avg_speed_mms.to_le_bytes());
    msgs.extend_from_slice(&max_speed_mms.to_le_bytes());
    msgs.extend_from_slice(&(ftp.min(0xFFFE) as u16).to_le_bytes());
    msgs.extend_from_slice(&tss.to_le_bytes());
    msgs.extend_from_slice(&intensity_factor.to_le_bytes());
    msgs.push(aerobic_te);
    msgs.push(anaerobic_te);
    msgs.extend_from_slice(&training_load_peak.to_le_bytes());
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
            (2, 1, 0x00),   // type: enum
            (3, 1, 0x00),   // event: enum
            (4, 1, 0x00),   // event_type: enum
        ],
    );
    msgs.push(0x04);
    msgs.extend_from_slice(&end_ts.to_le_bytes());
    msgs.extend_from_slice(&elapsed_ms.to_le_bytes());
    msgs.extend_from_slice(&end_ts.to_le_bytes()); // local_timestamp ≈ end_ts
    msgs.extend_from_slice(&1u16.to_le_bytes()); // num_sessions
    msgs.push(0); // type
    msgs.push(26); // event = activity
    msgs.push(1); // event_type = stop

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
                let mut altitude_m: Option<f32> = None;

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
                        // fitparser applies the scale/offset, so altitude arrives in metres.
                        "altitude" | "enhanced_altitude" => {
                            if let Some(v) = fit_f32(field.value()) {
                                if v.is_finite() {
                                    altitude_m = Some(v);
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
                        target_watts: None,
                        heart_rate_bpm: hr,
                        cadence_rpm: cad,
                        speed_kmh,
                        lat: lat_deg,
                        lng: lng_deg,
                        altitude_m,
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
        // Imported rides were not executed against app targets.
        ftp_watts: None,
        title: None,
        icu_id: None,
    })
}

/// Extract a u32 from common FIT numeric value variants.
/// Extract an f32 from common FIT numeric value variants.
fn fit_f32(value: &fitparser::Value) -> Option<f32> {
    match value {
        fitparser::Value::Float32(v) => Some(*v),
        fitparser::Value::Float64(v) => Some(*v as f32),
        fitparser::Value::SInt8(v) => Some(*v as f32),
        fitparser::Value::SInt16(v) => Some(*v as f32),
        fitparser::Value::SInt32(v) => Some(*v as f32),
        fitparser::Value::UInt8(v) => Some(*v as f32),
        fitparser::Value::UInt16(v) => Some(*v as f32),
        fitparser::Value::UInt32(v) => Some(*v as f32),
        _ => None,
    }
}

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

/// Write `session` to `path` as a FIT activity file.
///
/// Fails if the file cannot be created or written.
pub fn write_session_fit(path: &Path, session: &Session, athlete: &AthleteProfile) -> Result<()> {
    std::fs::write(path, encode_session(session, athlete))
        .with_context(|| format!("failed to write FIT file to {}", path.display()))
}

/// Write the FIT file to `~/.local/share/cycle/exports/` and return the path.
pub fn export_to_xdg_path(session: &Session, athlete: &AthleteProfile) -> Result<PathBuf> {
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
    std::fs::write(&path, encode_session(session, athlete))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(elapsed: u32, with_gps: bool) -> DataPoint {
        DataPoint {
            elapsed_secs: elapsed,
            power_watts: Some(200),
            target_watts: None,
            heart_rate_bpm: Some(145),
            cadence_rpm: Some(88),
            speed_kmh: Some(36.0), // 10 m/s — one metre per 0.1 s, easy to reason about
            lat: with_gps.then_some(51.5),
            lng: with_gps.then_some(-0.12),
            altitude_m: with_gps.then_some(100.0 + elapsed as f32),
        }
    }

    fn ride(points: u32, with_gps: bool) -> Session {
        let mut s = Session::new(None);
        s.ended_at = Some(s.started_at + chrono::Duration::seconds(points as i64));
        s.data_points = (0..points).map(|i| point(i, with_gps)).collect();
        s
    }

    fn profile() -> AthleteProfile {
        AthleteProfile {
            ftp_watts: 200,
            max_hr: 195,
            resting_hr: 52,
            ..AthleteProfile::default()
        }
    }

    fn encode(points: u32, with_gps: bool) -> Vec<u8> {
        encode_session(&ride(points, with_gps), &profile())
    }

    /// Decode the file and return the named field of the session message.
    fn session_field(bytes: &[u8], name: &str) -> Option<fitparser::Value> {
        let records = fitparser::from_bytes(bytes).expect("encoder must emit parseable FIT");
        records
            .iter()
            .find(|r| r.kind() == fitparser::profile::MesgNum::Session)
            .and_then(|r| {
                r.fields()
                    .iter()
                    .find(|f| f.name() == name)
                    .map(|f| f.value().clone())
            })
    }

    #[test]
    fn should_write_a_valid_fit_header() {
        let bytes = encode(10, true);
        assert_eq!(bytes[0], 14, "header size");
        assert_eq!(&bytes[8..12], b".FIT");
        // data_size must cover exactly the bytes between header and trailing CRC
        let data_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        assert_eq!(bytes.len(), 14 + data_size + 2);
    }

    #[test]
    fn should_end_with_a_crc_over_the_message_stream() {
        let bytes = encode(10, true);
        let msgs = &bytes[14..bytes.len() - 2];
        let stored = u16::from_le_bytes([bytes[bytes.len() - 2], bytes[bytes.len() - 1]]);
        assert_eq!(crc16(msgs), stored);
        let header_crc = u16::from_le_bytes([bytes[12], bytes[13]]);
        assert_eq!(crc16(&bytes[..12]), header_crc);
    }

    #[test]
    fn should_grow_by_one_record_per_data_point() {
        let short = encode(10, true).len();
        let long = encode(11, true).len();
        // 1 header byte + 24 bytes of record fields:
        // timestamp 4, power 2, hr 1, cadence 1, lat 4, lng 4, altitude 2, distance 4, speed 2
        assert_eq!(long - short, 25);
    }

    #[test]
    fn should_encode_position_as_semicircles() {
        let bytes = encode(1, true);
        let expected = (51.5 * (2_147_483_648.0 / 180.0)) as i32;
        assert!(
            bytes
                .windows(4)
                .any(|w| i32::from_le_bytes([w[0], w[1], w[2], w[3]]) == expected),
            "latitude not present in the encoded file"
        );
    }

    #[test]
    fn should_mark_a_missing_position_invalid() {
        let bytes = encode(1, false);
        assert!(
            bytes
                .windows(4)
                .any(|w| i32::from_le_bytes([w[0], w[1], w[2], w[3]]) == i32::MAX),
            "absent position was not written as the invalid sentinel"
        );
    }

    #[test]
    fn should_report_a_gps_ride_as_a_virtual_activity() {
        // sub_sport follows sport (2 = cycling) in the session message
        let with_gps = encode(5, true);
        let without = encode(5, false);
        assert!(with_gps.windows(2).any(|w| w == [2, 58]));
        assert!(without.windows(2).any(|w| w == [2, 6]));
    }

    #[test]
    fn should_accumulate_distance_from_speed() {
        // 10 points at 36 km/h = 10 m/s → 100 m → 10 000 cm
        let bytes = encode(10, true);
        assert!(
            bytes
                .windows(4)
                .any(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == 10_000),
            "total distance not present in the encoded file"
        );
    }

    #[test]
    fn should_round_trip_through_the_importer() {
        let original = ride(30, true);
        let dir = std::env::temp_dir().join("cycle-fit-roundtrip");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("test.fit");
        std::fs::write(&path, encode_session(&original, &profile())).expect("write");

        let parsed = import_fit_file(&path).expect("the file we wrote must parse");
        assert_eq!(parsed.data_points.len(), original.data_points.len());
        let first = &parsed.data_points[0];
        assert_eq!(first.power_watts, Some(200));
        assert_eq!(first.heart_rate_bpm, Some(145));
        assert_eq!(first.cadence_rpm, Some(88));
        let lat = first.lat.expect("latitude must survive the round trip");
        assert!((lat - 51.5).abs() < 0.0001, "got {lat}");
        let alt = first.altitude_m.expect("altitude must survive");
        assert!((alt - 100.0).abs() < 0.5, "got {alt}");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn should_identify_the_file_as_a_recognised_recording_device() {
        // Garmin Connect reads the training-load fields only from a file that
        // identifies as a device it recognises; under manufacturer 255 it threw
        // them away. Deliberate — see the constants at the top of this module.
        let records = fitparser::from_bytes(&encode(60, false)).expect("parses");
        let file_id = records
            .iter()
            .find(|r| r.kind() == fitparser::profile::MesgNum::FileId)
            .expect("file_id message");
        let field = |name: &str| {
            file_id
                .fields()
                .iter()
                .find(|f| f.name() == name)
                .map(|f| format!("{}", f.value()))
        };
        assert_eq!(field("manufacturer").as_deref(), Some("garmin"));
        assert_eq!(field("serial_number").as_deref(), Some("3444313785"));
        assert!(
            records
                .iter()
                .any(|r| r.kind() == fitparser::profile::MesgNum::FileCreator),
            "a device-written file always carries a file_creator message"
        );
    }

    #[test]
    fn should_carry_the_training_load_garmin_reads() {
        // Garmin Connect does not derive training load for a file it did not
        // record, so an export without these three fields contributes nothing
        // to training status however complete the rest of the ride is.
        let bytes = encode(1800, false);
        for field in [
            "total_training_effect",
            "total_anaerobic_training_effect",
            "training_load_peak",
        ] {
            assert!(
                session_field(&bytes, field).is_some(),
                "{field} missing from the session message"
            );
        }
    }

    #[test]
    fn should_scale_training_load_to_the_value_the_estimator_produced() {
        let ride = ride(1800, false);
        let expected = crate::training::load::estimate(&ride, &profile()).expect("has data");
        let bytes = encode_session(&ride, &profile());
        let Some(fitparser::Value::Float64(load)) = session_field(&bytes, "training_load_peak")
        else {
            panic!("training_load_peak must decode as a scaled number");
        };
        assert!(
            (load as f32 - expected.load).abs() < 0.5,
            "wrote {load}, estimated {}",
            expected.load
        );
    }

    #[test]
    fn should_report_the_ftp_the_ride_was_ridden_against() {
        let mut ride = ride(600, false);
        ride.ftp_watts = Some(240);
        let bytes = encode_session(&ride, &profile());
        assert_eq!(
            session_field(&bytes, "threshold_power"),
            Some(fitparser::Value::UInt16(240))
        );
    }

    #[test]
    fn should_close_the_recording_with_a_timer_stop_event() {
        // A file whose timer never stops reads as an unfinished recording.
        let records = fitparser::from_bytes(&encode(60, false)).expect("parses");
        let stops = records
            .iter()
            .filter(|r| r.kind() == fitparser::profile::MesgNum::Event)
            .filter(|r| {
                r.fields()
                    .iter()
                    .any(|f| f.name() == "event_type" && format!("{}", f.value()) == "stop_all")
            })
            .count();
        assert_eq!(stops, 1, "expected exactly one timer stop event");
    }

    #[test]
    fn should_declare_the_activity_as_a_completed_recording() {
        let records = fitparser::from_bytes(&encode(60, false)).expect("parses");
        let activity = records
            .iter()
            .find(|r| r.kind() == fitparser::profile::MesgNum::Activity)
            .expect("activity message");
        let field = |name: &str| {
            activity
                .fields()
                .iter()
                .find(|f| f.name() == name)
                .map(|f| format!("{}", f.value()))
        };
        // These three were previously written under each other's field numbers,
        // which left the file claiming an activity that had only just started.
        assert_eq!(field("event").as_deref(), Some("activity"));
        assert_eq!(field("event_type").as_deref(), Some("stop"));
        assert_eq!(field("num_sessions").as_deref(), Some("1"));
    }

    #[test]
    fn should_name_a_heart_rate_sensor_only_when_the_ride_recorded_one() {
        let hr_sensors = |bytes: &[u8]| {
            fitparser::from_bytes(bytes)
                .expect("parses")
                .iter()
                .filter(|r| r.kind() == fitparser::profile::MesgNum::DeviceInfo)
                // An ANT+ sensor resolves through the subfield, so the decoded
                // name is antplus_device_type rather than device_type.
                .filter(|r| {
                    r.fields().iter().any(|f| {
                        f.name().ends_with("device_type")
                            && matches!(format!("{}", f.value()).as_str(), "120" | "heart_rate")
                    })
                })
                .count()
        };
        assert_eq!(hr_sensors(&encode(60, false)), 1);

        let mut no_hr = ride(60, false);
        for p in &mut no_hr.data_points {
            p.heart_rate_bpm = None;
        }
        assert_eq!(hr_sensors(&encode_session(&no_hr, &profile())), 0);
    }

    #[test]
    fn should_build_a_filename_from_the_activity_name() {
        let name = suggested_filename(&ride(1, false), "Alpe d'Huez / lap 2");
        assert!(name.starts_with("Alpe_d_Huez___lap_2-"), "got {name}");
        assert!(name.ends_with(".fit"));
        assert!(!name.contains('/'), "path separators must not survive");
    }

    #[test]
    fn should_fall_back_to_ride_for_an_empty_name() {
        assert!(suggested_filename(&ride(1, false), "  ").starts_with("Ride-"));
    }
}
