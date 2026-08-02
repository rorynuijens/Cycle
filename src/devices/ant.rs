//! ANT+ FE-C (Fitness Equipment Control) trainer support over a USB ANT stick.
//!
//! Trainers that predate Bluetooth FTMS (e.g. the original Elite Drivo) can still
//! be controlled over ANT+ using the standardised FE-C profile. This module talks
//! to a Garmin/Dynastream ANT USB stick via libusb (`rusb`), brings up an ANT+
//! slave channel paired to an FE-C trainer, parses live data, and sends ERG target
//! power.
//!
//! Architecture: `rusb` is blocking, so [`run`] executes on a dedicated `std::thread`
//! (never the GLib or tokio loops). It speaks to the rest of the app exclusively via
//! channels — readings/connection events go out on the shared [`DeviceEvent`] sender,
//! commands come in on a plain `mpsc` receiver — mirroring the BLE manager boundary
//! (see CLAUDE.md §2.3). This file imports no GTK.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use async_channel::Sender;

use crate::data::session::{LiveReadings, ReadingSource};
use crate::devices::ftms::compute_cadence_rpm;
use crate::devices::manager::{DeviceEvent, DeviceType};
use crate::devices::peripheral::Transport;

/// Supported ANT USB stick USB IDs (Dynastream/Garmin).
const ANT_VID: u16 = 0x0fcf;
const ANT_PIDS: [u16; 2] = [0x1008, 0x1009]; // ANTUSB2, ANTUSB-m

/// Synthetic device address used so the ANT trainer flows through the same
/// discovery/connect pipeline as BLE devices in the Devices page.
pub const ANT_FEC_ADDRESS: &str = "ant:fec";

// ── ANT Message Protocol ─────────────────────────────────────────────────────
const ANT_SYNC: u8 = 0xA4;

const MSG_RESET_SYSTEM: u8 = 0x4A;
const MSG_SET_NETWORK_KEY: u8 = 0x46;
const MSG_ASSIGN_CHANNEL: u8 = 0x42;
const MSG_SET_CHANNEL_ID: u8 = 0x51;
const MSG_SET_CHANNEL_RF_FREQ: u8 = 0x45;
const MSG_SET_CHANNEL_PERIOD: u8 = 0x43;
const MSG_OPEN_CHANNEL: u8 = 0x4B;
const MSG_CLOSE_CHANNEL: u8 = 0x4C;
const MSG_BROADCAST_DATA: u8 = 0x4E;
const MSG_ACKNOWLEDGED_DATA: u8 = 0x4F;

/// ANT+ managed network key (public, published by Garmin for ANT+ device profiles).
const ANT_PLUS_NETWORK_KEY: [u8; 8] = [0xB9, 0xA5, 0x21, 0xFB, 0xBD, 0x72, 0xC3, 0x45];

const NETWORK_NUMBER: u8 = 0x00;
/// Bidirectional slave (receive) — lets us read broadcasts and send acknowledged control.
const CHANNEL_TYPE_SLAVE: u8 = 0x00;
const RF_FREQUENCY: u8 = 0x39; // 57 → 2457 MHz (ANT+)

/// Channel 0 — FE-C (Fitness Equipment Control): live data + ERG control.
const FEC_CHANNEL: u8 = 0x00;
const FEC_DEVICE_TYPE: u8 = 0x11; // 17 = Fitness Equipment Control
const FEC_CHANNEL_PERIOD: u16 = 8192; // 4 Hz (32768 / 8192)

/// Channel 1 — Bike Speed & Cadence: some trainers (e.g. Elite Drivo) report a bogus
/// cadence of 0 in both the FE-C and Bike Power pages, so we open this dedicated
/// channel and derive cadence from its crank-revolution counter instead.
const HR_CHANNEL: u8 = 0x02;
const HR_DEVICE_TYPE: u8 = 0x78; // 120 = Heart Rate Monitor
const HR_CHANNEL_PERIOD: u16 = 8070; // ANT+ HR profile period (~4.06 Hz)

const CADENCE_CHANNEL: u8 = 0x01;
const SC_DEVICE_TYPE: u8 = 0x79; // 121 = Bike Speed and Cadence (combined)
const SC_CHANNEL_PERIOD: u16 = 8086; // ANT+ combined Speed & Cadence period

// FE-C data page numbers
const PAGE_GENERAL_FE: u8 = 0x10; // 16
const PAGE_SPECIFIC_TRAINER: u8 = 0x19; // 25
const PAGE_TARGET_POWER: u8 = 0x31; // 49
const PAGE_TRACK_RESISTANCE: u8 = 0x33; // 51

/// Commands sent from the device manager to the ANT thread.
#[derive(Debug)]
pub enum AntCommand {
    Scan,
    Connect,
    Disconnect,
    SetTargetPower(u16),
    /// SIM mode: set the simulated road gradient in percent.
    SetTrackResistance(f32),
    SetErgMode(bool),
    Shutdown,
}

/// XOR checksum over every byte (including the sync byte) — the ANT frame trailer.
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc ^ b)
}

/// Encode an ANT message: `A4 | len | id | data… | checksum`.
fn encode_message(msg_id: u8, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(data.len() + 4);
    msg.push(ANT_SYNC);
    msg.push(data.len() as u8);
    msg.push(msg_id);
    msg.extend_from_slice(data);
    let cs = checksum(&msg);
    msg.push(cs);
    msg
}

/// Build the FE-C Data Page 49 (Target Power) payload for ERG mode.
///
/// Target power is transmitted in 0.25 W units (watts × 4) as a little-endian u16
/// in the last two bytes; bytes 1–5 are reserved (0xFF).
fn build_target_power_page(watts: u16) -> [u8; 8] {
    let quarter_watts = watts.saturating_mul(4);
    let [lsb, msb] = quarter_watts.to_le_bytes();
    [PAGE_TARGET_POWER, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, lsb, msb]
}

/// Build the FE-C Data Page 51 (Track Resistance) payload for SIM mode.
///
/// Grade is a little-endian u16 in 0.01 % units with a −200 % offset
/// (so 0 % ⇒ 20000 = 0x4E20). The final byte is the rolling resistance
/// coefficient in 5×10⁻⁵ units — 80 ⇒ 0.004, matching the virtual-speed
/// physics model. Grade is clamped to ±20 % — never trust raw route data
/// (CLAUDE.md §5.1).
fn build_track_resistance_page(grade_percent: f32) -> [u8; 8] {
    let raw = ((grade_percent.clamp(-20.0, 20.0) + 200.0) * 100.0).round() as u16;
    let [lsb, msb] = raw.to_le_bytes();
    [PAGE_TRACK_RESISTANCE, 0xFF, 0xFF, 0xFF, 0xFF, lsb, msb, 80]
}

/// Parse an 8-byte FE-C Specific Trainer Data page (0x19) into readings.
///
/// Instantaneous power is a 12-bit field; 0xFFF marks it invalid. Cadence 0xFF is
/// invalid. Returns `None` for the wrong page or a short payload.
fn parse_specific_trainer(payload: &[u8]) -> Option<LiveReadings> {
    if payload.len() < 8 || payload[0] != PAGE_SPECIFIC_TRAINER {
        return None;
    }
    let cadence = match payload[2] {
        0xFF => None,
        rpm => Some(rpm as u32),
    };
    let inst_power = (payload[5] as u16) | (((payload[6] & 0x0F) as u16) << 8);
    let power = if inst_power == 0x0FFF {
        None
    } else {
        Some((inst_power as u32).min(3000)) // clamp implausible values — CLAUDE.md §5.1
    };
    Some(LiveReadings {
        power_watts: power,
        cadence_rpm: cadence,
        source: ReadingSource::Ant,
        ..Default::default()
    })
}

/// Parse an 8-byte FE-C General FE Data page (0x10) for speed (0.001 m/s → km/h)
/// and heart rate. Returns `None` for the wrong page or a short payload.
fn parse_general_fe(payload: &[u8]) -> Option<LiveReadings> {
    if payload.len() < 8 || payload[0] != PAGE_GENERAL_FE {
        return None;
    }
    let speed_raw = (payload[4] as u32) | ((payload[5] as u32) << 8);
    let speed_kmh = if speed_raw == 0xFFFF {
        None
    } else {
        Some(speed_raw as f64 * 0.001 * 3.6)
    };
    let heart_rate_bpm = match payload[6] {
        0x00 | 0xFF => None,
        hr => Some(hr as u32),
    };
    Some(LiveReadings {
        speed_kmh: speed_kmh.map(|s| s as f32),
        heart_rate_bpm,
        source: ReadingSource::Ant,
        ..Default::default()
    })
}

/// Extract `(cumulative cadence revolutions, cadence event time)` from a combined
/// Computed heart rate from an ANT+ HR page (device type 120).
///
/// Every HR data page carries the computed heart rate in the last byte, whatever
/// the page number, so the page type never has to be decoded. Zero means the
/// monitor has no reading yet.
fn parse_ant_heart_rate(payload: &[u8]) -> Option<u32> {
    if payload.len() < 8 {
        return None;
    }
    match payload[7] {
        0 => None,
        bpm => Some((bpm as u32).min(250)), // clamp per CLAUDE.md §5.1
    }
}

/// ANT+ Bike Speed & Cadence page (device type 121). Cadence is bytes 0–3:
/// event time (1/1024 s) then revolution count, both little-endian. Cadence rpm is
/// derived from successive samples via [`compute_cadence_rpm`].
fn parse_speed_cadence(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 8 {
        return None;
    }
    let event_time = u16::from_le_bytes([payload[0], payload[1]]);
    let revolutions = u16::from_le_bytes([payload[2], payload[3]]);
    Some((revolutions, event_time))
}

// ── USB thread ───────────────────────────────────────────────────────────────

/// Endpoints + handle for an opened ANT stick.
struct AntStick {
    handle: rusb::DeviceHandle<rusb::GlobalContext>,
    in_ep: u8,
    out_ep: u8,
}

impl AntStick {
    /// Find and open the first ANT USB stick. Returns `None` if no stick is
    /// present or it can't be claimed (e.g. missing udev permissions).
    fn open() -> Option<Self> {
        let devices = rusb::devices().ok()?;
        for device in devices.iter() {
            let desc = match device.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.vendor_id() != ANT_VID || !ANT_PIDS.contains(&desc.product_id()) {
                continue;
            }
            let handle = match device.open() {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(
                        "ANT stick found ({:04x}:{:04x}) but could not be opened: {e} \
                         — check udev permissions",
                        desc.vendor_id(),
                        desc.product_id()
                    );
                    return None;
                }
            };

            // Locate bulk IN/OUT endpoints on the first interface.
            let (mut in_ep, mut out_ep) = (0x81u8, 0x01u8);
            if let Ok(config) = device.active_config_descriptor() {
                if let Some(iface) = config.interfaces().next() {
                    if let Some(desc) = iface.descriptors().next() {
                        for ep in desc.endpoint_descriptors() {
                            if ep.transfer_type() == rusb::TransferType::Bulk {
                                match ep.direction() {
                                    rusb::Direction::In => in_ep = ep.address(),
                                    rusb::Direction::Out => out_ep = ep.address(),
                                }
                            }
                        }
                    }
                }
            }

            let _ = handle.set_auto_detach_kernel_driver(true);
            if let Err(e) = handle.claim_interface(0) {
                tracing::warn!("ANT stick claim_interface failed: {e}");
                return None;
            }
            tracing::info!(
                "ANT stick opened ({:04x}:{:04x}) in=0x{in_ep:02x} out=0x{out_ep:02x}",
                desc.vendor_id(),
                desc.product_id()
            );
            return Some(Self {
                handle,
                in_ep,
                out_ep,
            });
        }
        None
    }

    fn write(&self, msg: &[u8]) {
        if let Err(e) = self
            .handle
            .write_bulk(self.out_ep, msg, Duration::from_millis(200))
        {
            tracing::warn!("ANT write failed: {e}");
        }
    }

    /// Read whatever bytes are available; returns an empty slice on timeout.
    fn read(&self, buf: &mut [u8]) -> usize {
        match self
            .handle
            .read_bulk(self.in_ep, buf, Duration::from_millis(50))
        {
            Ok(n) => n,
            Err(rusb::Error::Timeout) => 0,
            Err(e) => {
                tracing::debug!("ANT read: {e}");
                0
            }
        }
    }

    /// Configure and open one ANT+ slave channel, paired by wildcard to any device
    /// of `device_type`.
    fn open_channel(&self, channel: u8, device_type: u8, period: u16) {
        self.write(&encode_message(
            MSG_ASSIGN_CHANNEL,
            &[channel, CHANNEL_TYPE_SLAVE, NETWORK_NUMBER],
        ));
        // Device number 0,0 = wildcard.
        self.write(&encode_message(
            MSG_SET_CHANNEL_ID,
            &[channel, 0x00, 0x00, device_type, 0x00],
        ));
        self.write(&encode_message(
            MSG_SET_CHANNEL_RF_FREQ,
            &[channel, RF_FREQUENCY],
        ));
        let [plsb, pmsb] = period.to_le_bytes();
        self.write(&encode_message(
            MSG_SET_CHANNEL_PERIOD,
            &[channel, plsb, pmsb],
        ));
        self.write(&encode_message(MSG_OPEN_CHANNEL, &[channel]));
    }

    /// Reset the stick and bring up both the FE-C control channel and the Bike Power
    /// channel (the latter only to recover cadence on trainers that omit it from FE-C).
    fn open_channels(&self) {
        self.write(&encode_message(MSG_RESET_SYSTEM, &[0x00]));
        std::thread::sleep(Duration::from_millis(600));
        let mut key = vec![NETWORK_NUMBER];
        key.extend_from_slice(&ANT_PLUS_NETWORK_KEY);
        self.write(&encode_message(MSG_SET_NETWORK_KEY, &key));
        self.open_channel(FEC_CHANNEL, FEC_DEVICE_TYPE, FEC_CHANNEL_PERIOD);
        self.open_channel(CADENCE_CHANNEL, SC_DEVICE_TYPE, SC_CHANNEL_PERIOD);
        self.open_channel(HR_CHANNEL, HR_DEVICE_TYPE, HR_CHANNEL_PERIOD);
        tracing::info!("ANT channels opened (FE-C + Speed/Cadence + HR, searching)");
    }

    fn close_channels(&self) {
        self.write(&encode_message(MSG_CLOSE_CHANNEL, &[FEC_CHANNEL]));
        self.write(&encode_message(MSG_CLOSE_CHANNEL, &[CADENCE_CHANNEL]));
        self.write(&encode_message(MSG_CLOSE_CHANNEL, &[HR_CHANNEL]));
    }

    fn send_target_power(&self, watts: u16) {
        let clamped = watts.min(1000); // never send an unclamped ERG target — CLAUDE.md §5.1
        let page = build_target_power_page(clamped);
        let mut data = Vec::with_capacity(9);
        data.push(FEC_CHANNEL);
        data.extend_from_slice(&page);
        self.write(&encode_message(MSG_ACKNOWLEDGED_DATA, &data));
        tracing::debug!("ANT ERG target: {clamped}W");
    }

    fn send_track_resistance(&self, grade_percent: f32) {
        let page = build_track_resistance_page(grade_percent);
        let mut data = Vec::with_capacity(9);
        data.push(FEC_CHANNEL);
        data.extend_from_slice(&page);
        self.write(&encode_message(MSG_ACKNOWLEDGED_DATA, &data));
        tracing::debug!("ANT SIM grade: {grade_percent:.1}%");
    }
}

/// Iterate complete ANT frames in `buf`, invoking `f(msg_id, data)` for each.
fn for_each_frame(buf: &[u8], mut f: impl FnMut(u8, &[u8])) {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] != ANT_SYNC {
            i += 1;
            continue;
        }
        let len = buf[i + 1] as usize;
        let total = len + 4; // sync + len + id + data + checksum
        if i + total > buf.len() {
            break;
        }
        let frame = &buf[i..i + total];
        if checksum(&frame[..total - 1]) == frame[total - 1] {
            f(frame[2], &frame[3..3 + len]);
        }
        i += total;
    }
}

/// ANT thread entry point. Runs until a `Shutdown` command or the channel closes.
pub fn run(event_tx: Sender<DeviceEvent>, cmd_rx: Receiver<AntCommand>) {
    let mut stick: Option<AntStick> = None;
    let mut discovered = false;
    let mut connected = false;
    let mut channel_open = false;
    let mut erg_enabled = true;
    // Set once the Speed & Cadence channel supplies cadence; thereafter FE-C cadence
    // is ignored (some trainers report a bogus 0 there).
    let mut got_ant_cadence = false;
    // Previous (revolutions, event_time) sample from the cadence channel.
    let mut last_cadence_sample: Option<(u16, u16)> = None;
    let mut buf = [0u8; 512];

    loop {
        // Drain pending commands first.
        loop {
            match cmd_rx.try_recv() {
                Ok(AntCommand::Scan) => {
                    if stick.is_none() {
                        stick = AntStick::open();
                    }
                    match &stick {
                        Some(s) => {
                            discovered = false;
                            s.open_channels();
                            channel_open = true;
                        }
                        None => tracing::warn!("ANT scan requested but no stick available"),
                    }
                }
                Ok(AntCommand::Connect) => {
                    if stick.is_none() {
                        stick = AntStick::open();
                    }
                    if let Some(s) = &stick {
                        // The user may connect a saved ANT device without scanning first,
                        // so make sure the FE-C channel is up before going live.
                        if !channel_open {
                            s.open_channels();
                            channel_open = true;
                        }
                        connected = true;
                        let _ = event_tx.send_blocking(DeviceEvent::ConnectionChanged {
                            address: ANT_FEC_ADDRESS.to_string(),
                            connected: true,
                            device_type: Some(DeviceType::FtmsTrainer),
                        });
                        tracing::info!("ANT FE-C trainer connected");
                    } else {
                        tracing::warn!("ANT connect requested but no stick available");
                    }
                }
                Ok(AntCommand::Disconnect) => {
                    if let Some(s) = &stick {
                        s.close_channels();
                    }
                    connected = false;
                    discovered = false;
                    let _ = event_tx.send_blocking(DeviceEvent::ConnectionChanged {
                        address: ANT_FEC_ADDRESS.to_string(),
                        connected: false,
                        device_type: None,
                    });
                }
                Ok(AntCommand::SetTargetPower(watts)) => {
                    if connected && erg_enabled {
                        if let Some(s) = &stick {
                            s.send_target_power(watts);
                        }
                    }
                }
                Ok(AntCommand::SetTrackResistance(grade_percent)) => {
                    // Gated on the same "automatic resistance" switch as ERG —
                    // it is the user's kill switch for trainer control.
                    if connected && erg_enabled {
                        if let Some(s) = &stick {
                            s.send_track_resistance(grade_percent);
                        }
                    }
                }
                Ok(AntCommand::SetErgMode(enabled)) => {
                    erg_enabled = enabled;
                    tracing::info!("ANT ERG mode: {}", if enabled { "on" } else { "off" });
                }
                Ok(AntCommand::Shutdown) => {
                    if let Some(s) = &stick {
                        s.close_channels();
                    }
                    tracing::info!("ANT thread stopping");
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::info!("ANT command channel closed");
                    return;
                }
            }
        }

        // Pump USB if a channel is up; otherwise idle briefly.
        let Some(s) = &stick else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let n = s.read(&mut buf);
        if n == 0 {
            continue;
        }
        for_each_frame(&buf[..n], |msg_id, data| {
            if msg_id != MSG_BROADCAST_DATA || data.len() < 9 {
                return;
            }
            // data[0] = channel number, data[1..9] = 8-byte ANT+ page.
            let channel = data[0];
            let payload = &data[1..9];
            tracing::debug!(
                "ANT ch{channel} page 0x{:02x}: {:02x?}",
                payload[0],
                payload
            );

            // Heart rate channel — independent of the trainer, so it reports
            // whether or not an FE-C trainer has been found.
            if channel == HR_CHANNEL {
                if let Some(bpm) = parse_ant_heart_rate(payload) {
                    let _ = event_tx.send_blocking(DeviceEvent::Readings(LiveReadings {
                        heart_rate_bpm: Some(bpm),
                        source: ReadingSource::Ant,
                        ..Default::default()
                    }));
                }
                return;
            }

            // Speed & Cadence channel — used to recover cadence on trainers that
            // omit it from the FE-C trainer page. Cadence is derived from the change
            // in crank revolutions between samples.
            if channel == CADENCE_CHANNEL {
                if connected {
                    if let Some((revs, time)) = parse_speed_cadence(payload) {
                        if let Some((prev_revs, prev_time)) = last_cadence_sample {
                            if let Some(rpm) = compute_cadence_rpm(prev_revs, prev_time, revs, time)
                            {
                                got_ant_cadence = true;
                                let _ =
                                    event_tx.send_blocking(DeviceEvent::Readings(LiveReadings {
                                        cadence_rpm: Some(rpm),
                                        source: ReadingSource::Ant,
                                        ..Default::default()
                                    }));
                            }
                        }
                        last_cadence_sample = Some((revs, time));
                    }
                }
                return;
            }

            // FE-C channel: discovery + power / speed / HR.
            if !discovered {
                discovered = true;
                let _ = event_tx.send_blocking(DeviceEvent::PeripheralDiscovered {
                    address: ANT_FEC_ADDRESS.to_string(),
                    name: "ANT+ Trainer (FE-C)".to_string(),
                    rssi: None,
                    transport: Transport::AntPlus,
                    // FE-C is by definition a controllable trainer.
                    kind: DeviceType::FtmsTrainer,
                });
            }

            if !connected {
                return;
            }
            if let Some(mut readings) =
                parse_specific_trainer(payload).or_else(|| parse_general_fe(payload))
            {
                // Once cadence arrives on the Bike Power channel, defer to it.
                if got_ant_cadence {
                    readings.cadence_rpm = None;
                }
                let _ = event_tx.send_blocking(DeviceEvent::Readings(readings));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compute_xor_checksum() {
        // A4 ^ 01 ^ 4A ^ 00 = 0xEF
        assert_eq!(checksum(&[0xA4, 0x01, 0x4A, 0x00]), 0xEF);
    }

    #[test]
    fn should_encode_message_with_sync_len_and_checksum() {
        // Reset System: A4 01 4A 00 EF
        let msg = encode_message(MSG_RESET_SYSTEM, &[0x00]);
        assert_eq!(msg, vec![0xA4, 0x01, 0x4A, 0x00, 0xEF]);
    }

    #[test]
    fn should_build_target_power_page_in_quarter_watts() {
        // 200 W → 800 = 0x0320 little-endian
        let page = build_target_power_page(200);
        assert_eq!(page, [0x31, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x20, 0x03]);
    }

    #[test]
    fn should_build_track_resistance_page_for_flat_road() {
        // 0% → (0 + 200) × 100 = 20000 = 0x4E20 LE; Crr byte 80 = 0.004
        let page = build_track_resistance_page(0.0);
        assert_eq!(page, [0x33, 0xFF, 0xFF, 0xFF, 0xFF, 0x20, 0x4E, 80]);
    }

    #[test]
    fn should_build_track_resistance_page_for_climb_and_descent() {
        // 6% → 20600 = 0x5078 LE
        assert_eq!(
            build_track_resistance_page(6.0),
            [0x33, 0xFF, 0xFF, 0xFF, 0xFF, 0x78, 0x50, 80]
        );
        // −5% → 19500 = 0x4C2C LE
        assert_eq!(
            build_track_resistance_page(-5.0),
            [0x33, 0xFF, 0xFF, 0xFF, 0xFF, 0x2C, 0x4C, 80]
        );
    }

    #[test]
    fn should_clamp_track_resistance_grade() {
        // 45% clamps to 20% → 22000 = 0x55F0 LE
        assert_eq!(
            build_track_resistance_page(45.0),
            [0x33, 0xFF, 0xFF, 0xFF, 0xFF, 0xF0, 0x55, 80]
        );
    }

    #[test]
    fn should_parse_power_and_cadence_from_specific_trainer_page() {
        // page 25, cadence 90, accumulated 0, instantaneous power 250 (0x0FA)
        let payload = [0x19, 0x05, 0x5A, 0x00, 0x00, 0xFA, 0x00, 0x00];
        let r = parse_specific_trainer(&payload).unwrap();
        assert_eq!(r.power_watts, Some(250));
        assert_eq!(r.cadence_rpm, Some(90));
    }

    #[test]
    fn should_treat_invalid_power_and_cadence_as_none() {
        // cadence 0xFF (invalid), instantaneous power 0xFFF (invalid)
        let payload = [0x19, 0x05, 0xFF, 0x00, 0x00, 0xFF, 0x0F, 0x00];
        let r = parse_specific_trainer(&payload).unwrap();
        assert_eq!(r.power_watts, None);
        assert_eq!(r.cadence_rpm, None);
    }

    #[test]
    fn should_reconstruct_12bit_power() {
        // power = 0x123 = 291: lsb 0x23, msb nibble 0x1
        let payload = [0x19, 0x00, 0x50, 0x00, 0x00, 0x23, 0x01, 0x00];
        let r = parse_specific_trainer(&payload).unwrap();
        assert_eq!(r.power_watts, Some(291));
    }

    #[test]
    fn should_reject_wrong_page_for_specific_trainer() {
        assert!(parse_specific_trainer(&[0x10, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(parse_specific_trainer(&[0x19, 0, 0]).is_none());
    }

    #[test]
    fn should_extract_revs_and_time_from_speed_cadence_page() {
        // cadence event time 0x0400 (LE), cadence revs 0x0064 (LE) = 100
        let payload = [0x00, 0x04, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_speed_cadence(&payload), Some((100, 0x0400)));
    }

    #[test]
    fn should_derive_60_rpm_from_one_rev_per_second() {
        // 1 revolution in 1024 ticks (1 s) → 60 rpm, via compute_cadence_rpm
        let (r0, t0) = parse_speed_cadence(&[0x00, 0x00, 0x63, 0x00, 0, 0, 0, 0]).unwrap();
        let (r1, t1) = parse_speed_cadence(&[0x00, 0x04, 0x64, 0x00, 0, 0, 0, 0]).unwrap();
        assert_eq!(compute_cadence_rpm(r0, t0, r1, t1), Some(60));
    }

    #[test]
    fn should_parse_computed_heart_rate_from_an_ant_hr_page() {
        // Every HR page carries computed HR in the last byte, whatever the page.
        let page = [0x04, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x8c];
        assert_eq!(parse_ant_heart_rate(&page), Some(140));
    }

    #[test]
    fn should_ignore_an_ant_hr_page_with_no_reading_yet() {
        let page = [0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_ant_heart_rate(&page), None);
        assert_eq!(parse_ant_heart_rate(&[0x04]), None);
    }

    #[test]
    fn should_clamp_an_implausible_ant_heart_rate() {
        let page = [0x04, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff];
        assert_eq!(parse_ant_heart_rate(&page), Some(250));
    }

    #[test]
    fn should_parse_heart_rate_from_general_page() {
        // page 16, speed invalid, HR 140
        let payload = [0x10, 0x19, 0x00, 0x00, 0xFF, 0xFF, 0x8C, 0x00];
        let r = parse_general_fe(&payload).unwrap();
        assert_eq!(r.heart_rate_bpm, Some(140));
        assert_eq!(r.power_watts, None);
    }

    #[test]
    fn should_extract_two_frames_from_one_buffer() {
        let mut buf = encode_message(MSG_RESET_SYSTEM, &[0x00]);
        buf.extend(encode_message(MSG_OPEN_CHANNEL, &[0x00]));
        let mut ids = Vec::new();
        for_each_frame(&buf, |id, _| ids.push(id));
        assert_eq!(ids, vec![MSG_RESET_SYSTEM, MSG_OPEN_CHANNEL]);
    }
}
