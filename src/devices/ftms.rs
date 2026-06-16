/// Fitness Machine Service (FTMS) — BLE GATT profile for smart trainers.
#[allow(dead_code)]
pub const FTMS_SERVICE_UUID: &str = "00001826-0000-1000-8000-00805f9b34fb";
pub const INDOOR_BIKE_DATA_UUID: &str = "00002ad2-0000-1000-8000-00805f9b34fb";
pub const CONTROL_POINT_UUID: &str = "00002ad9-0000-1000-8000-00805f9b34fb";

/// Heart Rate Service — BLE GATT profile for HR monitors and bands.
#[allow(dead_code)]
pub const HR_SERVICE_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
pub const HR_MEASUREMENT_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";

/// Cycling Power Service (CPS) — BLE GATT profile for power meters.
#[allow(dead_code)]
pub const CYCLING_POWER_SERVICE_UUID: &str = "00001818-0000-1000-8000-00805f9b34fb";
pub const CYCLING_POWER_MEASUREMENT_UUID: &str = "00002a63-0000-1000-8000-00805f9b34fb";

/// Cycling Speed and Cadence Service (CSC) — BLE GATT profile for cadence/speed sensors.
#[allow(dead_code)]
pub const CSC_SERVICE_UUID: &str = "00001816-0000-1000-8000-00805f9b34fb";

/// Parse a Heart Rate Measurement GATT notification (Bluetooth Assigned Numbers §3.104).
/// Supports both uint8 (bit 0 = 0) and uint16 (bit 0 = 1) heart rate formats.
pub fn parse_hr_measurement(data: &[u8]) -> Option<u32> {
    if data.is_empty() {
        return None;
    }
    let hr = if data[0] & 0x01 == 0 {
        *data.get(1)? as u32
    } else {
        if data.len() < 3 {
            return None;
        }
        u16::from_le_bytes([data[1], data[2]]) as u32
    };
    Some(hr.clamp(0, 250))
}

#[derive(Debug, Clone, Default)]
pub struct IndoorBikeData {
    pub speed_kmh: Option<f32>,
    pub cadence_rpm: Option<u32>,
    pub power_watts: Option<u32>,
    pub heart_rate_bpm: Option<u32>,
    pub elapsed_time_secs: Option<u32>,
    #[allow(dead_code)]
    pub distance_m: Option<u32>,
}

/// Parse raw bytes from an Indoor Bike Data GATT notification.
pub fn parse_indoor_bike_data(data: &[u8]) -> Option<IndoorBikeData> {
    if data.len() < 2 {
        return None;
    }

    let flags = u16::from_le_bytes([data[0], data[1]]);
    let mut result = IndoorBikeData::default();
    let mut offset = 2usize;

    // Bit 0 = 0 means speed field IS present
    if flags & 0x0001 == 0 && offset + 2 <= data.len() {
        result.speed_kmh =
            Some(u16::from_le_bytes([data[offset], data[offset + 1]]) as f32 / 100.0);
        offset += 2;
    }

    // Bit 2: instantaneous cadence
    if flags & 0x0004 != 0 && offset + 2 <= data.len() {
        result.cadence_rpm = Some(u16::from_le_bytes([data[offset], data[offset + 1]]) as u32 / 2);
        offset += 2;
    }

    // Bit 6: instantaneous power
    if flags & 0x0040 != 0 && offset + 2 <= data.len() {
        result.power_watts = Some(u16::from_le_bytes([data[offset], data[offset + 1]]) as u32);
        offset += 2;
    }

    // Bit 9: heart rate
    if flags & 0x0200 != 0 && offset < data.len() {
        result.heart_rate_bpm = Some(data[offset] as u32);
        offset += 1;
    }

    // Bit 11: elapsed time
    if flags & 0x0800 != 0 && offset + 2 <= data.len() {
        result.elapsed_time_secs =
            Some(u16::from_le_bytes([data[offset], data[offset + 1]]) as u32);
    }

    Some(result)
}

/// Parsed fields from a Cycling Power Measurement GATT notification (BT Spec §3.67).
#[derive(Debug, Clone, Default)]
pub struct CyclingPowerData {
    /// Instantaneous power in watts (signed in the spec; clamped to 0..=3000 on use).
    pub power_watts: i32,
    /// Cumulative crank revolutions (uint16, wraps at 65535).
    pub crank_revs: Option<u16>,
    /// Timestamp of the last crank event (uint16, units: 1/1024 s, wraps at 65535).
    pub crank_event_time: Option<u16>,
}

/// Parse raw bytes from a Cycling Power Measurement GATT notification.
///
/// Instantaneous power (sint16) is mandatory and always at bytes 2–3.
/// Optional fields are skipped according to the flags bitmask so that
/// crank revolution data is located correctly regardless of what other
/// optional fields precede it.
pub fn parse_cycling_power_measurement(data: &[u8]) -> Option<CyclingPowerData> {
    if data.len() < 4 {
        return None;
    }
    let flags = u16::from_le_bytes([data[0], data[1]]);
    let power = i16::from_le_bytes([data[2], data[3]]) as i32;

    let mut result = CyclingPowerData {
        power_watts: power,
        ..Default::default()
    };
    let mut offset = 4usize;

    // Bit 0: Pedal Power Balance Present (1 byte)
    if flags & 0x0001 != 0 {
        offset += 1;
    }
    // Bit 2: Accumulated Torque Present (2 bytes)
    if flags & 0x0004 != 0 {
        offset += 2;
    }
    // Bit 4: Wheel Revolution Data Present (4 bytes cumulative + 2 bytes event time = 6)
    if flags & 0x0010 != 0 {
        offset += 6;
    }
    // Bit 5: Crank Revolution Data Present (2 bytes cumulative revs + 2 bytes event time)
    if flags & 0x0020 != 0 && offset + 4 <= data.len() {
        result.crank_revs = Some(u16::from_le_bytes([data[offset], data[offset + 1]]));
        result.crank_event_time = Some(u16::from_le_bytes([data[offset + 2], data[offset + 3]]));
    }

    Some(result)
}

/// Compute cadence in RPM from two successive Crank Revolution Data samples.
///
/// Both counters are uint16 and wrap; wrapping subtraction handles rollover.
/// Returns `None` when the last crank event time is unchanged (crank not moving
/// or first measurement — no cadence can be derived).
pub fn compute_cadence_rpm(
    prev_revs: u16,
    prev_event_time: u16,
    curr_revs: u16,
    curr_event_time: u16,
) -> Option<u32> {
    let delta_time = curr_event_time.wrapping_sub(prev_event_time) as u32; // 1/1024 s units
    if delta_time == 0 {
        return None; // no new crank event since last packet
    }
    let delta_revs = curr_revs.wrapping_sub(prev_revs) as u32;
    // cadence = (revolutions / time_s) × 60 = (delta_revs × 1024 × 60) / delta_time
    let rpm = (delta_revs * 1024 * 60) / delta_time;
    Some(rpm.clamp(0, 250))
}

/// OpCode 0x05 — Set Target Power (ERG mode), little-endian i16.
pub fn set_target_power_command(watts: u16) -> Vec<u8> {
    let bytes = watts.to_le_bytes();
    vec![0x05, bytes[0], bytes[1]]
}

/// OpCode 0x00 — Request Control (must be sent before any other command).
pub fn request_control_command() -> Vec<u8> {
    vec![0x00]
}

/// OpCode 0x01 — Reset machine to default state.
#[allow(dead_code)]
pub fn reset_command() -> Vec<u8> {
    vec![0x01]
}

/// OpCode 0x07 — Start or Resume.
///
/// Many FTMS trainers (e.g. Elite) sit in the *Idle* state after Request Control
/// and silently ignore Set Target Power (0x05) until they have been moved into the
/// *Started* state. This command performs that transition so ERG targets take effect.
pub fn start_resume_command() -> Vec<u8> {
    vec![0x07]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_uint8_hr_measurement() {
        // Flags byte 0x00: bit 0 = 0 → uint8 format. 0x5C = 92 bpm.
        assert_eq!(parse_hr_measurement(&[0x00, 0x5C]), Some(92));
    }

    #[test]
    fn should_parse_uint16_hr_measurement() {
        // Flags byte 0x01: bit 0 = 1 → uint16 format, little-endian. 0x005C = 92 bpm.
        assert_eq!(parse_hr_measurement(&[0x01, 0x5C, 0x00]), Some(92));
    }

    #[test]
    fn should_return_none_for_empty_hr_packet() {
        assert_eq!(parse_hr_measurement(&[]), None);
    }

    #[test]
    fn should_return_none_for_truncated_uint16_packet() {
        assert_eq!(parse_hr_measurement(&[0x01, 0x5C]), None);
    }

    #[test]
    fn should_clamp_implausible_hr_to_250() {
        // 0xFF = 255 bpm — physiologically impossible, clamp to 250.
        assert_eq!(parse_hr_measurement(&[0x00, 0xFF]), Some(250));
    }

    // ── Cycling Power Measurement ────────────────────────────────────────────

    #[test]
    fn should_parse_power_only_from_cpp_packet() {
        // Flags = 0x0000 (no optional fields), Power = 200 W (0x00C8 LE)
        let data = &[0x00, 0x00, 0xC8, 0x00];
        let result = parse_cycling_power_measurement(data).unwrap();
        assert_eq!(result.power_watts, 200);
        assert!(result.crank_revs.is_none());
        assert!(result.crank_event_time.is_none());
    }

    #[test]
    fn should_parse_power_and_crank_data_from_cpp_packet() {
        // Flags = 0x0020 (bit 5: crank data present)
        // Power = 250 W (0x00FA LE), Crank revs = 100 (0x0064 LE), Event time = 2048 (0x0800 LE)
        let data = &[0x20, 0x00, 0xFA, 0x00, 0x64, 0x00, 0x00, 0x08];
        let result = parse_cycling_power_measurement(data).unwrap();
        assert_eq!(result.power_watts, 250);
        assert_eq!(result.crank_revs, Some(100));
        assert_eq!(result.crank_event_time, Some(0x0800));
    }

    #[test]
    fn should_return_none_for_too_short_cpp_packet() {
        assert!(parse_cycling_power_measurement(&[0x00, 0x00, 0xC8]).is_none());
    }

    #[test]
    fn should_compute_60_rpm_from_one_rev_per_second() {
        // 1 revolution in exactly 1024 ticks (1 second) → 60 rpm
        assert_eq!(compute_cadence_rpm(99, 1024, 100, 2048), Some(60));
    }

    #[test]
    fn should_compute_90_rpm_from_crank_data() {
        // 3 revolutions in 2048 ticks (2 seconds) → 90 rpm
        assert_eq!(compute_cadence_rpm(97, 0, 100, 2048), Some(90));
    }

    #[test]
    fn should_return_none_cadence_when_crank_not_moving() {
        // Same event time: no new crank event, cadence unknown
        assert_eq!(compute_cadence_rpm(100, 2048, 100, 2048), None);
    }

    #[test]
    fn should_handle_crank_counter_wraparound() {
        // Counter wraps from 65535 to 1 = 2 revolutions
        assert_eq!(compute_cadence_rpm(65535, 0, 1, 2048), Some(60));
    }

    // ── Control Point commands ───────────────────────────────────────────────

    #[test]
    fn should_build_request_control_command() {
        assert_eq!(request_control_command(), vec![0x00]);
    }

    #[test]
    fn should_build_start_resume_command() {
        assert_eq!(start_resume_command(), vec![0x07]);
    }

    #[test]
    fn should_build_correct_set_target_power_command() {
        // 300 = 0x012C, little-endian
        assert_eq!(set_target_power_command(300), vec![0x05, 0x2C, 0x01]);
    }
}
