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

use crate::data::session::LiveReadings;
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
const CHANNEL: u8 = 0x00;
/// Bidirectional slave (receive) — lets us read broadcasts and send acknowledged control.
const CHANNEL_TYPE_SLAVE: u8 = 0x00;
const FEC_DEVICE_TYPE: u8 = 0x11; // 17 = Fitness Equipment Control
const FEC_RF_FREQUENCY: u8 = 0x39; // 57 → 2457 MHz
const FEC_CHANNEL_PERIOD: u16 = 8192; // 4 Hz (32768 / 8192)

// FE-C data page numbers
const PAGE_GENERAL_FE: u8 = 0x10; // 16
const PAGE_SPECIFIC_TRAINER: u8 = 0x19; // 25
const PAGE_TARGET_POWER: u8 = 0x31; // 49

/// Commands sent from the device manager to the ANT thread.
#[derive(Debug)]
pub enum AntCommand {
    Scan,
    Connect,
    Disconnect,
    SetTargetPower(u16),
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
        ..Default::default()
    })
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

    /// Bring up the ANT+ FE-C slave channel paired to any nearby trainer (wildcard).
    fn open_fec_channel(&self) {
        self.write(&encode_message(MSG_RESET_SYSTEM, &[0x00]));
        std::thread::sleep(Duration::from_millis(600));
        self.write(&encode_message(
            MSG_SET_NETWORK_KEY,
            &[
                NETWORK_NUMBER,
                ANT_PLUS_NETWORK_KEY[0],
                ANT_PLUS_NETWORK_KEY[1],
                ANT_PLUS_NETWORK_KEY[2],
                ANT_PLUS_NETWORK_KEY[3],
                ANT_PLUS_NETWORK_KEY[4],
                ANT_PLUS_NETWORK_KEY[5],
                ANT_PLUS_NETWORK_KEY[6],
                ANT_PLUS_NETWORK_KEY[7],
            ],
        ));
        self.write(&encode_message(
            MSG_ASSIGN_CHANNEL,
            &[CHANNEL, CHANNEL_TYPE_SLAVE, NETWORK_NUMBER],
        ));
        // Device number 0,0 = wildcard (pair with any FE-C trainer).
        self.write(&encode_message(
            MSG_SET_CHANNEL_ID,
            &[CHANNEL, 0x00, 0x00, FEC_DEVICE_TYPE, 0x00],
        ));
        self.write(&encode_message(
            MSG_SET_CHANNEL_RF_FREQ,
            &[CHANNEL, FEC_RF_FREQUENCY],
        ));
        let [plsb, pmsb] = FEC_CHANNEL_PERIOD.to_le_bytes();
        self.write(&encode_message(
            MSG_SET_CHANNEL_PERIOD,
            &[CHANNEL, plsb, pmsb],
        ));
        self.write(&encode_message(MSG_OPEN_CHANNEL, &[CHANNEL]));
        tracing::info!("ANT FE-C channel opened (searching)");
    }

    fn close_channel(&self) {
        self.write(&encode_message(MSG_CLOSE_CHANNEL, &[CHANNEL]));
    }

    fn send_target_power(&self, watts: u16) {
        let clamped = watts.min(1000); // never send an unclamped ERG target — CLAUDE.md §5.1
        let page = build_target_power_page(clamped);
        let mut data = Vec::with_capacity(9);
        data.push(CHANNEL);
        data.extend_from_slice(&page);
        self.write(&encode_message(MSG_ACKNOWLEDGED_DATA, &data));
        tracing::debug!("ANT ERG target: {clamped}W");
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
    let mut erg_enabled = true;
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
                            s.open_fec_channel();
                        }
                        None => tracing::warn!("ANT scan requested but no stick available"),
                    }
                }
                Ok(AntCommand::Connect) => {
                    if stick.is_some() {
                        connected = true;
                        let _ = event_tx.send_blocking(DeviceEvent::ConnectionChanged {
                            address: ANT_FEC_ADDRESS.to_string(),
                            connected: true,
                            device_type: Some(DeviceType::FtmsTrainer),
                        });
                        tracing::info!("ANT FE-C trainer connected");
                    }
                }
                Ok(AntCommand::Disconnect) => {
                    if let Some(s) = &stick {
                        s.close_channel();
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
                Ok(AntCommand::SetErgMode(enabled)) => {
                    erg_enabled = enabled;
                    tracing::info!("ANT ERG mode: {}", if enabled { "on" } else { "off" });
                }
                Ok(AntCommand::Shutdown) => {
                    if let Some(s) = &stick {
                        s.close_channel();
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
            // data[0] = channel, data[1..9] = 8-byte FE-C page.
            let payload = &data[1..9];

            // First valid trainer broadcast surfaces the device in the Devices page.
            if !discovered {
                discovered = true;
                let _ = event_tx.send_blocking(DeviceEvent::PeripheralDiscovered {
                    address: ANT_FEC_ADDRESS.to_string(),
                    name: "ANT+ Trainer (FE-C)".to_string(),
                    rssi: None,
                    transport: Transport::AntPlus,
                });
            }

            if !connected {
                return;
            }
            if let Some(readings) =
                parse_specific_trainer(payload).or_else(|| parse_general_fe(payload))
            {
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
