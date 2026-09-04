/// Fitness Machine Service (FTMS) — BLE GATT profile for smart trainers.
pub const FTMS_SERVICE_UUID: &str = "00001826-0000-1000-8000-00805f9b34fb";
pub const INDOOR_BIKE_DATA_UUID: &str = "00002ad2-0000-1000-8000-00805f9b34fb";
pub const CONTROL_POINT_UUID: &str = "00002ad9-0000-1000-8000-00805f9b34fb";

/// Heart Rate Service — BLE GATT profile for HR monitors and bands.
pub const HR_SERVICE_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
pub const HR_MEASUREMENT_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";

/// Cycling Power Service (CPS) — BLE GATT profile for power meters.
pub const CYCLING_POWER_SERVICE_UUID: &str = "00001818-0000-1000-8000-00805f9b34fb";
pub const CYCLING_POWER_MEASUREMENT_UUID: &str = "00002a63-0000-1000-8000-00805f9b34fb";

/// Cycling Speed and Cadence Service (CSC) — BLE GATT profile for cadence/speed sensors.
pub const CSC_SERVICE_UUID: &str = "00001816-0000-1000-8000-00805f9b34fb";
pub const CSC_MEASUREMENT_UUID: &str = "00002a5b-0000-1000-8000-00805f9b34fb";

// Ceilings for readings coming off the radio (CLAUDE.md §5.1). A trainer that
// glitches or lies sends a full-scale uint16, and an unclamped one is written
// into the ride and then into TSS, CTL, the FIT export and the coach prompt —
// where it cannot be told from a real effort. Every parser below clamps at the
// point of parsing so no caller can forget to.
/// Well above any human capability.
pub const MAX_PLAUSIBLE_POWER_W: u32 = 3000;
/// The physical maximum for a pedalling cadence.
pub const MAX_PLAUSIBLE_CADENCE_RPM: u32 = 250;
/// The medical maximum for a heart rate.
pub const MAX_PLAUSIBLE_HR_BPM: u32 = 250;
/// Comfortably above any speed a bicycle reaches, paced or descending.
///
/// Speed is the one reading this app never measures itself — both transports
/// take the trainer's word for it — so a full-scale uint16 arrives as 655 km/h
/// and a mangled one as anything at all. It is fed to distance and to the
/// average in the ride summary, where a single bad sample does not average out.
pub const MAX_PLAUSIBLE_SPEED_KMH: f32 = 150.0;

/// Ceiling on an ERG target this app will ask a trainer to hold (CLAUDE.md §5.1).
///
/// Unlike the ceilings above, this one guards the write rather than the read: it
/// bounds what leaves for the hardware, not what arrives from it. It lives here,
/// beside them, so the BLE and ANT+ paths cannot drift to different limits.
pub const MAX_ERG_TARGET_W: u16 = 1000;

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
    Some(hr.min(MAX_PLAUSIBLE_HR_BPM))
}

#[derive(Debug, Clone, Default)]
pub struct IndoorBikeData {
    pub speed_kmh: Option<f32>,
    pub cadence_rpm: Option<u32>,
    pub power_watts: Option<u32>,
    pub heart_rate_bpm: Option<u32>,
    pub elapsed_time_secs: Option<u32>,
    pub distance_m: Option<u32>,
}

/// A cursor over the variable-length body of an Indoor Bike Data packet.
///
/// The fields appear in flag-bit order with no length prefixes and no padding,
/// so the only way to find one is to step over every field before it. A field
/// this app has no use for still has to be consumed, or everything after it is
/// read from the wrong offset.
struct Fields<'a> {
    data: &'a [u8],
    offset: usize,
}

impl Fields<'_> {
    /// Consume `n` bytes, or `None` when the packet ends first.
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.offset.checked_add(n)?;
        let field = self.data.get(self.offset..end)?;
        self.offset = end;
        Some(field)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Option<i16> {
        self.take(2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }

    /// A 24-bit little-endian unsigned field (total distance is the only one).
    fn u24(&mut self) -> Option<u32> {
        self.take(3)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0]))
    }
}

/// Parse raw bytes from an Indoor Bike Data GATT notification
/// (FTMS spec §4.9.1).
///
/// Speed, power, cadence and heart rate are clamped to physiologically
/// plausible ranges — the trainer is untrusted hardware (CLAUDE.md §5.1).
///
/// Returns `None` only for a packet too short to hold the flags. A packet that
/// ends part-way through its declared fields keeps whatever was read before the
/// truncation: those fields were still at the right offset, and dropping a live
/// power reading because the trailer was short loses real data.
pub fn parse_indoor_bike_data(data: &[u8]) -> Option<IndoorBikeData> {
    if data.len() < 2 {
        return None;
    }
    let flags = u16::from_le_bytes([data[0], data[1]]);
    let mut fields = Fields {
        data,
        offset: 2, // past the flags
    };
    let mut result = IndoorBikeData::default();
    let _ = read_fields(flags, &mut fields, &mut result);
    Some(result)
}

/// Walk the packet body in field order, filling in what this app uses.
///
/// Returns `None` at the first field that runs off the end of the packet, which
/// stops the walk — a caller cannot meaningfully carry on past a truncation,
/// since every later offset would be a guess.
fn read_fields(flags: u16, f: &mut Fields, out: &mut IndoorBikeData) -> Option<()> {
    // Bit 0 is "More Data": unlike every other flag, the field is present when
    // the bit is *clear*.
    if flags & 0x0001 == 0 {
        out.speed_kmh = Some((f.u16()? as f32 / 100.0).min(MAX_PLAUSIBLE_SPEED_KMH));
    }
    if flags & 0x0002 != 0 {
        f.u16()?; // average speed
    }
    // Instantaneous cadence, uint16 at 0.5 rpm resolution.
    if flags & 0x0004 != 0 {
        out.cadence_rpm = Some((f.u16()? as u32 / 2).min(MAX_PLAUSIBLE_CADENCE_RPM));
    }
    if flags & 0x0008 != 0 {
        f.u16()?; // average cadence
    }
    if flags & 0x0010 != 0 {
        out.distance_m = Some(f.u24()?);
    }
    if flags & 0x0020 != 0 {
        f.i16()?; // resistance level
    }
    // Instantaneous power is signed: a trainer may report a negative watt or two
    // while coasting, which is not a reading worth keeping, so it floors at 0.
    if flags & 0x0040 != 0 {
        out.power_watts = Some((f.i16()?.max(0) as u32).min(MAX_PLAUSIBLE_POWER_W));
    }
    if flags & 0x0080 != 0 {
        f.i16()?; // average power
    }
    if flags & 0x0100 != 0 {
        f.take(5)?; // expended energy: total (u16) + per hour (u16) + per minute (u8)
    }
    if flags & 0x0200 != 0 {
        out.heart_rate_bpm = Some((f.u8()? as u32).min(MAX_PLAUSIBLE_HR_BPM));
    }
    if flags & 0x0400 != 0 {
        f.u8()?; // metabolic equivalent
    }
    if flags & 0x0800 != 0 {
        out.elapsed_time_secs = Some(f.u16()? as u32);
    }
    // Remaining time (bit 12) is the last field, so nothing depends on stepping
    // over it and nothing here uses it.
    Some(())
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

/// Parsed fields from a CSC Measurement GATT notification (BT Spec §3.55).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CscData {
    /// Cumulative crank revolutions (uint16, wraps at 65535).
    pub crank_revs: Option<u16>,
    /// Timestamp of the last crank event (uint16, units: 1/1024 s, wraps at 65535).
    pub crank_event_time: Option<u16>,
}

/// Parse raw bytes from a CSC Measurement GATT notification.
///
/// Flags (uint8): bit 0 = Wheel Revolution Data present (6 bytes, skipped —
/// speed needs a wheel circumference we don't know, and the trainer already
/// reports speed), bit 1 = Crank Revolution Data present (4 bytes).
/// Crank event time uses the same 1/1024 s units as the Cycling Power Service,
/// so [`compute_cadence_rpm`] works for both.
pub fn parse_csc_measurement(data: &[u8]) -> Option<CscData> {
    if data.is_empty() {
        return None;
    }
    let flags = data[0];
    let mut result = CscData::default();
    let mut offset = 1usize;

    // Bit 0: Wheel Revolution Data (uint32 cumulative revs + uint16 event time)
    if flags & 0x01 != 0 {
        offset += 6;
    }
    // Bit 1: Crank Revolution Data (uint16 cumulative revs + uint16 event time)
    if flags & 0x02 != 0 && offset + 4 <= data.len() {
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
    Some(rpm.min(MAX_PLAUSIBLE_CADENCE_RPM))
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

/// OpCode 0x11 — Set Indoor Bike Simulation Parameters (SIM mode).
///
/// Field layout: wind speed (sint16, 0.001 m/s), grade (sint16, 0.01 %),
/// rolling resistance coefficient (uint8, 0.0001), wind resistance
/// coefficient (uint8, 0.01 kg/m). Wind is fixed at zero; Crr/CdA match the
/// virtual-speed physics model in `data::route` (0.004 / ~0.51 kg/m).
/// Grade is clamped to ±20 % — never trust raw route data (CLAUDE.md §5.1).
pub fn set_simulation_command(grade_percent: f32) -> Vec<u8> {
    let grade = (grade_percent.clamp(-20.0, 20.0) * 100.0).round() as i16;
    let g = grade.to_le_bytes();
    vec![0x11, 0x00, 0x00, g[0], g[1], 40, 51]
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

    // ── Indoor Bike Data sanitisation (CLAUDE.md §5.1) ───────────────────────

    /// Flags 0x0245, little-endian: bit 2 (cadence), bit 6 (power), bit 9 (HR),
    /// and bit 0 *set*, which per the spec means no speed field precedes them.
    const FLAGS_CADENCE_POWER_HR: [u8; 2] = [0x45, 0x02];

    #[test]
    fn should_clamp_implausible_power_to_3000_watts() {
        // 0x7FFF = 32767 W, the largest a sint16 can claim, from a lying or
        // glitching trainer.
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0x00,
            0x00, // cadence 0
            0xFF,
            0x7F, // power 32767
            0x64, // HR 100
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(3000));
    }

    #[test]
    fn should_floor_a_negative_power_reading_at_zero() {
        // Instantaneous power is sint16 in the spec, so 0xFFFF is −1 W (a
        // coasting trainer), not 65535 W. Reading it unsigned turned a rider
        // freewheeling into a full-scale reading that clamped to 3000 W.
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0x00,
            0x00,
            0xFF,
            0xFF, // power −1
            0x64,
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(0));
    }

    #[test]
    fn should_pass_through_power_at_the_3000_watt_boundary() {
        // 3000 W = 0x0BB8 LE — a real sprint peak must survive untouched.
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0x00,
            0x00,
            0xB8,
            0x0B,
            0x64,
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(3000));
    }

    #[test]
    fn should_pass_through_a_realistic_power_reading() {
        // 280 W = 0x0118 LE.
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0x00,
            0x00,
            0x18,
            0x01,
            0x64,
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(280));
    }

    #[test]
    fn should_clamp_implausible_cadence_to_250_rpm() {
        // 0xFFFF at 0.5 rpm resolution = 32767 rpm.
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0xFF,
            0xFF,
            0x18,
            0x01,
            0x64,
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.cadence_rpm, Some(250));
    }

    #[test]
    fn should_clamp_implausible_hr_from_indoor_bike_data_to_250() {
        let data = &[
            FLAGS_CADENCE_POWER_HR[0],
            FLAGS_CADENCE_POWER_HR[1],
            0x00,
            0x00,
            0x18,
            0x01,
            0xFF, // 255 bpm
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.heart_rate_bpm, Some(250));
    }

    // ── Indoor Bike Data field alignment ─────────────────────────────────────
    //
    // The fields are positional: one the app does not use still has to be
    // stepped over, or every field after it is read from the wrong offset and
    // the wrong number lands in the ride, the TSS and the FIT export.

    #[test]
    fn should_parse_power_from_known_ftms_packet() {
        // The worked example in CLAUDE.md §3.4, kept executable so the standards
        // doc and the parser cannot drift apart again.
        // Flags 0x0040: bit 6 (power), bit 0 clear (speed present).
        let data = &[0x40, 0x00, 0xB8, 0x0B, 0x18, 0x01];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(280));
        assert_eq!(result.speed_kmh, Some(30.0));
    }

    #[test]
    fn should_read_power_past_an_average_speed_field() {
        // Flags 0x0043: bit 1 (average speed) and bit 6 (power), with bit 0 set
        // so no instantaneous speed leads. Skipping the average-speed field read
        // power from its bytes instead — 0x1388 — and reported a clamped 3000 W.
        let data = &[
            0x43, 0x00, // flags
            0x88, 0x13, // average speed 50.00 km/h
            0x18, 0x01, // power 280 W
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(280));
    }

    #[test]
    fn should_step_over_every_field_it_does_not_use() {
        // Flags 0x1FFE: every optional field present, and bit 0 clear so the
        // instantaneous speed field leads. This is the widest packet the spec
        // allows, and the one where a missed field is most costly.
        let data = &[
            0xFE, 0x1F, // flags
            0xB8, 0x0B, // instantaneous speed 30.00 km/h
            0x00, 0x00, // average speed
            0xB4, 0x00, // instantaneous cadence 180 → 90 rpm
            0x00, 0x00, // average cadence
            0x39, 0x30, 0x00, // total distance 12345 m
            0x00, 0x00, // resistance level
            0x18, 0x01, // instantaneous power 280 W
            0x00, 0x00, // average power
            0x00, 0x00, 0x00, 0x00, 0x00, // expended energy
            0x96, // heart rate 150 bpm
            0x00, // metabolic equivalent
            0x10, 0x0E, // elapsed time 3600 s
            0x00, 0x00, // remaining time
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.speed_kmh, Some(30.0));
        assert_eq!(result.cadence_rpm, Some(90));
        assert_eq!(result.distance_m, Some(12_345));
        assert_eq!(result.power_watts, Some(280));
        assert_eq!(result.heart_rate_bpm, Some(150));
        assert_eq!(result.elapsed_time_secs, Some(3600));
    }

    #[test]
    fn should_keep_the_fields_read_before_a_truncation() {
        // Flags promise speed, cadence and power, but the packet stops after
        // cadence. Speed and cadence were still read at the right offsets.
        let data = &[
            0x44, 0x00, // flags: cadence + power, speed present
            0xB8, 0x0B, // speed 30.00 km/h
            0xB4, 0x00, // cadence 90 rpm
        ];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.speed_kmh, Some(30.0));
        assert_eq!(result.cadence_rpm, Some(90));
        assert_eq!(result.power_watts, None);
    }

    #[test]
    fn should_return_none_for_a_packet_too_short_to_hold_flags() {
        assert!(parse_indoor_bike_data(&[]).is_none());
        assert!(parse_indoor_bike_data(&[0x00]).is_none());
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

    // ── CSC Measurement ──────────────────────────────────────────────────────

    #[test]
    fn should_parse_crank_data_from_csc_packet() {
        // Flags = 0x02 (crank data only)
        // Crank revs = 100 (0x0064 LE), Event time = 2048 (0x0800 LE)
        let data = &[0x02, 0x64, 0x00, 0x00, 0x08];
        let result = parse_csc_measurement(data).unwrap();
        assert_eq!(result.crank_revs, Some(100));
        assert_eq!(result.crank_event_time, Some(0x0800));
    }

    #[test]
    fn should_skip_wheel_data_to_reach_crank_data_in_csc_packet() {
        // Flags = 0x03 (wheel + crank data)
        // Wheel: revs = 5000 (uint32 LE), event time = 1024 — 6 bytes, skipped
        // Crank: revs = 200 (0x00C8 LE), event time = 4096 (0x1000 LE)
        let data = &[
            0x03, 0x88, 0x13, 0x00, 0x00, 0x00, 0x04, 0xC8, 0x00, 0x00, 0x10,
        ];
        let result = parse_csc_measurement(data).unwrap();
        assert_eq!(result.crank_revs, Some(200));
        assert_eq!(result.crank_event_time, Some(0x1000));
    }

    #[test]
    fn should_return_no_crank_data_for_wheel_only_csc_packet() {
        // Flags = 0x01 (wheel data only)
        let data = &[0x01, 0x88, 0x13, 0x00, 0x00, 0x00, 0x04];
        let result = parse_csc_measurement(data).unwrap();
        assert_eq!(result.crank_revs, None);
        assert_eq!(result.crank_event_time, None);
    }

    #[test]
    fn should_return_none_for_empty_csc_packet() {
        assert!(parse_csc_measurement(&[]).is_none());
    }

    #[test]
    fn should_ignore_truncated_crank_data_in_csc_packet() {
        // Flags claim crank data but only 2 of 4 bytes present
        let result = parse_csc_measurement(&[0x02, 0x64, 0x00]).unwrap();
        assert_eq!(result.crank_revs, None);
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

    #[test]
    fn should_build_simulation_command_for_positive_grade() {
        // 6% → 600 (0x0258 LE); wind 0; Crr 40; Cw 51
        assert_eq!(
            set_simulation_command(6.0),
            vec![0x11, 0x00, 0x00, 0x58, 0x02, 40, 51]
        );
    }

    #[test]
    fn should_build_simulation_command_for_descent() {
        // −5% → −500 as i16 LE = 0x0C 0xFE
        assert_eq!(
            set_simulation_command(-5.0),
            vec![0x11, 0x00, 0x00, 0x0C, 0xFE, 40, 51]
        );
    }

    #[test]
    fn should_clamp_simulation_grade_to_plus_minus_20_percent() {
        // 45% clamps to 20% → 2000 (0x07D0 LE)
        assert_eq!(
            set_simulation_command(45.0),
            vec![0x11, 0x00, 0x00, 0xD0, 0x07, 40, 51]
        );
        // −45% clamps to −20% → −2000 as i16 LE = 0x30 0xF8
        assert_eq!(
            set_simulation_command(-45.0),
            vec![0x11, 0x00, 0x00, 0x30, 0xF8, 40, 51]
        );
    }

    #[test]
    fn should_clamp_an_implausible_speed_to_150_kmh() {
        // Flags 0x0000: bit 0 clear, so instantaneous speed is present.
        // 0xFFFF = 655.35 km/h — a full-scale field from a trainer that lost
        // the plot. Found by the property test below, not by anyone's guess.
        let data = &[0x00, 0x00, 0xFF, 0xFF];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.speed_kmh, Some(MAX_PLAUSIBLE_SPEED_KMH));
    }

    #[test]
    fn should_keep_a_real_speed_untouched() {
        // 0x0BB8 = 3000 → 30.00 km/h, well inside the ceiling.
        let data = &[0x00, 0x00, 0xB8, 0x0B];
        assert_eq!(parse_indoor_bike_data(data).unwrap().speed_kmh, Some(30.0));
    }
}

/// Generative tests over the BLE parsing and command boundary.
///
/// The example-based tests above check the packets someone thought to write
/// down. These search for the ones nobody did: every parser here is fed bytes
/// from untrusted hardware (CLAUDE.md §5.1), and the properties asserted are the
/// two that must hold for *every* input — it never panics, and nothing it
/// returns is outside the range its documentation promises.
#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Packets as a glitching or hostile trainer might send them: any length up
    /// to a little over the longest real Indoor Bike Data packet, any contents.
    fn any_packet() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..40)
    }

    /// Grades including the values a route file should never contain but might.
    fn any_grade() -> impl Strategy<Value = f32> {
        prop_oneof![
            8 => -100.0f32..100.0f32,
            1 => Just(f32::NAN),
            1 => Just(f32::INFINITY),
            1 => Just(f32::NEG_INFINITY),
            1 => Just(f32::MAX),
            1 => Just(f32::MIN),
        ]
    }

    proptest! {
        /// No packet, however malformed or truncated, may panic a parser.
        #[test]
        fn should_never_panic_on_any_packet(data in any_packet()) {
            let _ = parse_indoor_bike_data(&data);
            let _ = parse_hr_measurement(&data);
            let _ = parse_cycling_power_measurement(&data);
            let _ = parse_csc_measurement(&data);
        }

        /// Every reading Indoor Bike Data yields is inside its ceiling.
        ///
        /// This is the property the header comment on those constants claims:
        /// clamping happens at the parse, so no caller can forget to. It held
        /// for power, cadence and heart rate and not for speed, which is how
        /// [`MAX_PLAUSIBLE_SPEED_KMH`] came to exist.
        #[test]
        fn should_clamp_every_indoor_bike_reading(data in any_packet()) {
            let Some(d) = parse_indoor_bike_data(&data) else { return Ok(()) };
            if let Some(w) = d.power_watts {
                prop_assert!(w <= MAX_PLAUSIBLE_POWER_W, "power {w}");
            }
            if let Some(c) = d.cadence_rpm {
                prop_assert!(c <= MAX_PLAUSIBLE_CADENCE_RPM, "cadence {c}");
            }
            if let Some(h) = d.heart_rate_bpm {
                prop_assert!(h <= MAX_PLAUSIBLE_HR_BPM, "hr {h}");
            }
            if let Some(v) = d.speed_kmh {
                prop_assert!(v.is_finite(), "speed {v} is not finite");
                prop_assert!((0.0..=MAX_PLAUSIBLE_SPEED_KMH).contains(&v), "speed {v}");
            }
        }

        /// A heart rate strap is untrusted in both of its packet formats.
        #[test]
        fn should_clamp_every_heart_rate(data in any_packet()) {
            if let Some(hr) = parse_hr_measurement(&data) {
                prop_assert!(hr <= MAX_PLAUSIBLE_HR_BPM, "hr {hr}");
            }
        }

        /// Two successive crank samples can never imply a superhuman cadence,
        /// whatever the counters do — including wrapping, or going backwards.
        #[test]
        fn should_clamp_every_derived_cadence(
            pr in any::<u16>(), pt in any::<u16>(),
            cr in any::<u16>(), ct in any::<u16>(),
        ) {
            if let Some(rpm) = compute_cadence_rpm(pr, pt, cr, ct) {
                prop_assert!(rpm <= MAX_PLAUSIBLE_CADENCE_RPM, "cadence {rpm}");
            }
        }

        /// The SIM grade that leaves for the trainer stays inside ±20 %,
        /// for every grade a route could hand it — NaN and infinity included.
        #[test]
        fn should_clamp_every_simulation_grade(grade in any_grade()) {
            let cmd = set_simulation_command(grade);
            prop_assert_eq!(cmd.len(), 7);
            let encoded = i16::from_le_bytes([cmd[3], cmd[4]]);
            prop_assert!((-2000..=2000).contains(&encoded), "grade encoded as {}", encoded);
        }

        /// An ERG target inside the ceiling survives encoding unchanged — the
        /// clamp belongs to the caller that reaches the hardware, so this
        /// guards the encoding rather than the limit.
        #[test]
        fn should_encode_every_erg_target_losslessly(watts in 0u16..=MAX_ERG_TARGET_W) {
            let cmd = set_target_power_command(watts);
            prop_assert_eq!(cmd.len(), 3);
            prop_assert_eq!(cmd[0], 0x05);
            prop_assert_eq!(u16::from_le_bytes([cmd[1], cmd[2]]), watts);
        }
    }
}
