use anyhow::Result;
use async_channel::{Receiver, Sender};
use std::collections::HashMap;

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as BtleManager, Peripheral as BtlePeriph,
    ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;

use crate::data::session::LiveReadings;
use crate::devices::ftms::{
    compute_cadence_rpm, parse_csc_measurement, parse_cycling_power_measurement,
    parse_hr_measurement, parse_indoor_bike_data, request_control_command,
    set_target_power_command, start_resume_command, CONTROL_POINT_UUID, CSC_MEASUREMENT_UUID,
    CSC_SERVICE_UUID, CYCLING_POWER_MEASUREMENT_UUID, CYCLING_POWER_SERVICE_UUID,
    FTMS_SERVICE_UUID, HR_MEASUREMENT_UUID, HR_SERVICE_UUID, INDOOR_BIKE_DATA_UUID,
};
use crate::devices::peripheral::Transport;

/// Commands sent from the UI thread to the Device Manager.
#[derive(Debug)]
#[allow(dead_code)]
pub enum DeviceCommand {
    StartScan,
    StopScan,
    Connect {
        address: String,
    },
    Disconnect {
        address: String,
    },
    SetTargetPower {
        watts: u16,
    },
    /// Enable or disable ERG mode (automatic resistance control) for the connected trainer.
    SetErgMode(bool),
}

/// The GATT profile a device was identified as, either from its advertised
/// services at scan time or from its characteristics at connect time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    FtmsTrainer,
    CyclingPowerMeter,
    HeartRateMonitor,
    CadenceSensor,
    Unknown,
}

impl DeviceType {
    /// Plain-language name shown to the user (novice-friendly, no protocol jargon).
    pub fn label(&self) -> &'static str {
        match self {
            Self::FtmsTrainer => "Smart trainer",
            Self::CyclingPowerMeter => "Power meter",
            Self::HeartRateMonitor => "Heart rate monitor",
            Self::CadenceSensor => "Cadence sensor",
            Self::Unknown => "Sensor",
        }
    }

    /// Symbolic icon representing the device's role.
    pub fn icon_name(&self) -> &'static str {
        match self {
            Self::FtmsTrainer => "preferences-system-symbolic",
            Self::CyclingPowerMeter => "power-profile-performance-symbolic",
            Self::HeartRateMonitor => "emblem-favorite-symbolic",
            Self::CadenceSensor => "media-playlist-repeat-symbolic",
            Self::Unknown => "bluetooth-symbolic",
        }
    }

    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::FtmsTrainer => "trainer",
            Self::CyclingPowerMeter => "power",
            Self::HeartRateMonitor => "hr",
            Self::CadenceSensor => "cadence",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "trainer" => Self::FtmsTrainer,
            "power" => Self::CyclingPowerMeter,
            "hr" => Self::HeartRateMonitor,
            "cadence" => Self::CadenceSensor,
            _ => Self::Unknown,
        }
    }
}

/// Events broadcast from the Device Manager back to the UI thread.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    PeripheralDiscovered {
        address: String,
        name: String,
        rssi: Option<i16>,
        transport: Transport,
        /// Role derived from the advertised GATT services (or the ANT+ profile).
        kind: DeviceType,
    },
    ConnectionChanged {
        address: String,
        connected: bool,
        /// Set when connecting; `None` on disconnect.
        device_type: Option<DeviceType>,
    },
    Readings(LiveReadings),
    Error(String),
    /// Non-fatal issue worth surfacing to the user (e.g. a trainer with no ERG control).
    Warning(String),
}

/// Runs in a background tokio task.
/// The UI communicates exclusively via channels — GTK's main loop stays separate.
pub struct DeviceManager {
    cmd_rx: Receiver<DeviceCommand>,
    event_tx: Sender<DeviceEvent>,
}

impl DeviceManager {
    pub fn new() -> (Self, Sender<DeviceCommand>, Receiver<DeviceEvent>) {
        let (cmd_tx, cmd_rx) = async_channel::bounded(64);
        let (event_tx, event_rx) = async_channel::bounded(256);
        (Self { cmd_rx, event_tx }, cmd_tx, event_rx)
    }

    pub async fn run(self) -> Result<()> {
        tracing::info!("Device manager started");

        // Attempt to get a BLE adapter; non-fatal if the machine has none.
        let adapter: Option<Adapter> = match Manager::new().await {
            Ok(manager) => match manager.adapters().await {
                Ok(adapters) => {
                    let a = adapters.into_iter().next();
                    if a.is_none() {
                        tracing::warn!("No BLE adapter found — BLE disabled");
                    } else {
                        tracing::info!("BLE adapter ready");
                    }
                    a
                }
                Err(e) => {
                    tracing::warn!("Failed to list BLE adapters: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("Failed to init BLE manager: {e}");
                None
            }
        };

        // Internal channel: a spawned task forwards btleplug CentralEvents here.
        let (ble_tx, mut ble_rx) = tokio::sync::mpsc::channel::<CentralEvent>(64);

        // State owned by the select loop (all non-Send, kept on this task).
        let mut discovered: HashMap<String, Peripheral> = HashMap::new();
        let mut trainer: Option<Peripheral> = None;
        let mut trainer_address: Option<String> = None;
        let mut ctrl_char: Option<Characteristic> = None;
        let mut power_meter: Option<Peripheral> = None;
        let mut power_meter_address: Option<String> = None;
        let mut hr_monitor: Option<Peripheral> = None;
        let mut hr_monitor_address: Option<String> = None;
        let mut cadence_sensor: Option<Peripheral> = None;
        let mut cadence_sensor_address: Option<String> = None;
        // ERG mode: only send SetTargetPower to the trainer when enabled.
        // Default true; overridden by SetErgMode commands from the UI.
        let mut erg_enabled = true;

        // ── ANT+ trainer support ──────────────────────────────────────────────
        // The ANT stick is driven by blocking libusb I/O, so it lives on its own
        // std::thread and communicates over channels — readings/connection events
        // come back on event_tx (shared with BLE), commands go out on ant_tx.
        let (ant_tx, ant_rx) = std::sync::mpsc::channel::<crate::devices::ant::AntCommand>();
        let ant_event_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::devices::ant::run(ant_event_tx, ant_rx));
        // Tracks whether an ANT trainer is the active ERG target, so the BLE path
        // doesn't log a spurious "no trainer connected" warning every second.
        let mut ant_connected = false;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    let Ok(cmd) = cmd else {
                        tracing::info!("Device manager stopping");
                        let _ = ant_tx.send(crate::devices::ant::AntCommand::Shutdown);
                        break;
                    };

                    match cmd {
                        DeviceCommand::StartScan => {
                            // Scan ANT+ alongside BLE (works even without a BLE adapter).
                            let _ = ant_tx.send(crate::devices::ant::AntCommand::Scan);
                            let Some(ref adapter) = adapter else {
                                tracing::warn!("StartScan: no BLE adapter available");
                                continue;
                            };
                            match adapter.start_scan(ScanFilter::default()).await {
                                Ok(_) => {
                                    tracing::info!("BLE scan started");
                                    // Spawn a task that forwards CentralEvents to our select loop.
                                    let adapter = adapter.clone();
                                    let tx = ble_tx.clone();
                                    tokio::spawn(async move {
                                        match adapter.events().await {
                                            Ok(mut stream) => {
                                                while let Some(ev) = stream.next().await {
                                                    if tx.send(ev).await.is_err() { break; }
                                                }
                                            }
                                            Err(e) => tracing::error!("BLE event stream: {e}"),
                                        }
                                    });
                                }
                                Err(e) => {
                                    tracing::error!("start_scan failed: {e}");
                                    let _ = self.event_tx.send(DeviceEvent::Error(e.to_string())).await;
                                }
                            }
                        }

                        DeviceCommand::StopScan => {
                            if let Some(ref adapter) = adapter {
                                adapter.stop_scan().await.ok();
                                tracing::info!("BLE scan stopped");
                            }
                        }

                        DeviceCommand::Connect { address } => {
                            // ANT+ devices are handled by the ANT thread, not btleplug.
                            if address.starts_with("ant:") {
                                let _ = ant_tx.send(crate::devices::ant::AntCommand::Connect);
                                ant_connected = true;
                                continue;
                            }
                            // BlueZ removes Connect() from the D-Bus interface while a
                            // scan is active — stop scanning before attempting to connect.
                            if let Some(ref adapter) = adapter {
                                adapter.stop_scan().await.ok();
                            }

                            let peripheral = match discovered.get(&address) {
                                Some(p) => p.clone(),
                                None => {
                                    tracing::warn!("Connect: unknown address {address}");
                                    continue;
                                }
                            };

                            tracing::info!("Connecting to {address}");
                            if let Err(e) = peripheral.connect().await {
                                tracing::error!("connect() failed: {e}");
                                let _ = self.event_tx.send(DeviceEvent::Error(e.to_string())).await;
                                continue;
                            }
                            if let Err(e) = peripheral.discover_services().await {
                                tracing::error!("discover_services() failed: {e}");
                                continue;
                            }

                            let chars = peripheral.characteristics();
                            // Diagnostic: list every characteristic the device exposes so we
                            // can see why a trainer may not bind to the FTMS branch.
                            for c in &chars {
                                tracing::debug!(
                                    "  char {} (service {})",
                                    c.uuid, c.service_uuid
                                );
                            }
                            let data_char = chars.iter()
                                .find(|c| c.uuid.to_string() == INDOOR_BIKE_DATA_UUID)
                                .cloned();
                            let found_ctrl = chars.iter()
                                .find(|c| c.uuid.to_string() == CONTROL_POINT_UUID)
                                .cloned();
                            let hr_char = chars.iter()
                                .find(|c| c.uuid.to_string() == HR_MEASUREMENT_UUID)
                                .cloned();
                            let power_char = chars.iter()
                                .find(|c| c.uuid.to_string() == CYCLING_POWER_MEASUREMENT_UUID)
                                .cloned();
                            let csc_char = chars.iter()
                                .find(|c| c.uuid.to_string() == CSC_MEASUREMENT_UUID)
                                .cloned();

                            let mut connected_as = DeviceType::Unknown;

                            // A device is treated as a controllable trainer when it exposes the
                            // FTMS Control Point or Indoor Bike Data. Some trainers report their
                            // live data via the Cycling Power Service while staying controllable
                            // through the FTMS Control Point, so control-point detection — not the
                            // data characteristic — decides whether ERG is available.
                            if found_ctrl.is_some() || data_char.is_some() {
                                // ── FTMS trainer (may also carry HR / cycling-power data) ──
                                if let Some(ref ch) = found_ctrl {
                                    // FTMS control sequence: take control, then move the
                                    // machine into the Started state so it honours ERG
                                    // (Set Target Power) commands — see ftms.rs.
                                    peripheral
                                        .write(ch, &request_control_command(), WriteType::WithResponse)
                                        .await
                                        .ok();
                                    peripheral
                                        .write(ch, &start_resume_command(), WriteType::WithResponse)
                                        .await
                                        .ok();
                                } else {
                                    tracing::warn!(
                                        "Trainer {address} exposes no Control Point — ERG mode will not work"
                                    );
                                    let _ = self.event_tx.send(DeviceEvent::Warning(
                                        "Trainer connected without ERG control — resistance can't be set automatically".to_string(),
                                    )).await;
                                }
                                // Subscribe to whatever data the trainer offers. Prefer Indoor
                                // Bike Data; fall back to the Cycling Power Service for trainers
                                // that only report there.
                                if let Some(ref ch) = data_char {
                                    if let Err(e) = peripheral.subscribe(ch).await {
                                        tracing::error!("subscribe(indoor_bike_data) failed: {e}");
                                    }
                                } else if let Some(ref ch) = power_char {
                                    if let Err(e) = peripheral.subscribe(ch).await {
                                        tracing::error!("subscribe(cycling_power) failed: {e}");
                                    }
                                }
                                // Also subscribe to HR if the trainer exposes it directly.
                                if let Some(ref ch) = hr_char {
                                    peripheral.subscribe(ch).await.ok();
                                }

                                let p = peripheral.clone();
                                let event_tx = self.event_tx.clone();
                                tokio::spawn(async move {
                                    let mut last_revs: Option<u16> = None;
                                    let mut last_time: Option<u16> = None;
                                    match p.notifications().await {
                                        Ok(mut stream) => {
                                            while let Some(n) = stream.next().await {
                                                let uuid = n.uuid.to_string();
                                                if uuid == INDOOR_BIKE_DATA_UUID {
                                                    if let Some(data) = parse_indoor_bike_data(&n.value) {
                                                        let readings = LiveReadings {
                                                            power_watts:             data.power_watts,
                                                            heart_rate_bpm:          data.heart_rate_bpm,
                                                            cadence_rpm:             data.cadence_rpm,
                                                            speed_kmh:               data.speed_kmh,
                                                            resistance_target_watts: None,
                                                        };
                                                        if event_tx.send(DeviceEvent::Readings(readings)).await.is_err() {
                                                            break;
                                                        }
                                                    }
                                                } else if uuid == CYCLING_POWER_MEASUREMENT_UUID {
                                                    if let Some(cpp) = parse_cycling_power_measurement(&n.value) {
                                                        let cadence = match (last_revs, last_time, cpp.crank_revs, cpp.crank_event_time) {
                                                            (Some(pr), Some(pt), Some(cr), Some(ct)) => {
                                                                compute_cadence_rpm(pr, pt, cr, ct)
                                                            }
                                                            _ => None,
                                                        };
                                                        last_revs = cpp.crank_revs;
                                                        last_time = cpp.crank_event_time;
                                                        let readings = LiveReadings {
                                                            power_watts: Some(cpp.power_watts.clamp(0, 3000) as u32),
                                                            cadence_rpm: cadence,
                                                            ..Default::default()
                                                        };
                                                        if event_tx.send(DeviceEvent::Readings(readings)).await.is_err() {
                                                            break;
                                                        }
                                                    }
                                                } else if uuid == HR_MEASUREMENT_UUID {
                                                    if let Some(hr) = parse_hr_measurement(&n.value) {
                                                        let readings = LiveReadings {
                                                            heart_rate_bpm: Some(hr),
                                                            ..Default::default()
                                                        };
                                                        if event_tx.send(DeviceEvent::Readings(readings)).await.is_err() {
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => tracing::error!("notifications() failed: {e}"),
                                    }
                                });

                                ctrl_char = found_ctrl;
                                trainer = Some(peripheral);
                                trainer_address = Some(address.clone());
                                connected_as = DeviceType::FtmsTrainer;
                                tracing::info!("FTMS trainer connected: {address}");

                            } else if let Some(power_ch) = power_char {
                                // ── Cycling Power Service (standalone power meter) ────────
                                if let Err(e) = peripheral.subscribe(&power_ch).await {
                                    tracing::error!("subscribe(cycling_power) failed: {e}");
                                } else {
                                    let p = peripheral.clone();
                                    let event_tx = self.event_tx.clone();
                                    tokio::spawn(async move {
                                        let mut last_revs: Option<u16> = None;
                                        let mut last_time: Option<u16> = None;
                                        match p.notifications().await {
                                            Ok(mut stream) => {
                                                while let Some(n) = stream.next().await {
                                                    if n.uuid.to_string()
                                                        != CYCLING_POWER_MEASUREMENT_UUID
                                                    {
                                                        continue;
                                                    }
                                                    let Some(cpp) =
                                                        parse_cycling_power_measurement(&n.value)
                                                    else {
                                                        continue;
                                                    };
                                                    let cadence = match (
                                                        last_revs,
                                                        last_time,
                                                        cpp.crank_revs,
                                                        cpp.crank_event_time,
                                                    ) {
                                                        (Some(pr), Some(pt), Some(cr), Some(ct)) => {
                                                            compute_cadence_rpm(pr, pt, cr, ct)
                                                        }
                                                        _ => None,
                                                    };
                                                    last_revs = cpp.crank_revs;
                                                    last_time = cpp.crank_event_time;
                                                    let readings = LiveReadings {
                                                        power_watts: Some(
                                                            cpp.power_watts.clamp(0, 3000) as u32,
                                                        ),
                                                        cadence_rpm: cadence,
                                                        ..Default::default()
                                                    };
                                                    if event_tx
                                                        .send(DeviceEvent::Readings(readings))
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("notifications() failed: {e}")
                                            }
                                        }
                                    });
                                    power_meter = Some(peripheral);
                                    power_meter_address = Some(address.clone());
                                    connected_as = DeviceType::CyclingPowerMeter;
                                    tracing::info!("Cycling power meter connected: {address}");
                                }

                            } else if let Some(hr_ch) = hr_char {
                                // ── Dedicated heart rate monitor (no FTMS) ──────────────
                                if let Err(e) = peripheral.subscribe(&hr_ch).await {
                                    tracing::error!("subscribe(hr_measurement) failed: {e}");
                                } else {
                                    let p = peripheral.clone();
                                    let event_tx = self.event_tx.clone();
                                    tokio::spawn(async move {
                                        match p.notifications().await {
                                            Ok(mut stream) => {
                                                while let Some(n) = stream.next().await {
                                                    if n.uuid.to_string() != HR_MEASUREMENT_UUID {
                                                        continue;
                                                    }
                                                    if let Some(hr) = parse_hr_measurement(&n.value) {
                                                        let readings = LiveReadings {
                                                            heart_rate_bpm: Some(hr),
                                                            ..Default::default()
                                                        };
                                                        if event_tx.send(DeviceEvent::Readings(readings)).await.is_err() {
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => tracing::error!("notifications() failed: {e}"),
                                        }
                                    });
                                    hr_monitor = Some(peripheral);
                                    hr_monitor_address = Some(address.clone());
                                    connected_as = DeviceType::HeartRateMonitor;
                                    tracing::info!("HR monitor connected: {address}");
                                }

                            } else if let Some(csc_ch) = csc_char {
                                // ── Dedicated cadence/speed sensor (CSC, no power) ──────
                                if let Err(e) = peripheral.subscribe(&csc_ch).await {
                                    tracing::error!("subscribe(csc_measurement) failed: {e}");
                                } else {
                                    let p = peripheral.clone();
                                    let event_tx = self.event_tx.clone();
                                    tokio::spawn(async move {
                                        // Cadence needs two successive crank samples; the
                                        // counters are cumulative and wrap (see ftms.rs).
                                        let mut last_revs: Option<u16> = None;
                                        let mut last_time: Option<u16> = None;
                                        match p.notifications().await {
                                            Ok(mut stream) => {
                                                while let Some(n) = stream.next().await {
                                                    if n.uuid.to_string() != CSC_MEASUREMENT_UUID {
                                                        continue;
                                                    }
                                                    let Some(csc) = parse_csc_measurement(&n.value)
                                                    else {
                                                        continue;
                                                    };
                                                    let cadence = match (
                                                        last_revs,
                                                        last_time,
                                                        csc.crank_revs,
                                                        csc.crank_event_time,
                                                    ) {
                                                        (Some(pr), Some(pt), Some(cr), Some(ct)) => {
                                                            compute_cadence_rpm(pr, pt, cr, ct)
                                                        }
                                                        _ => None,
                                                    };
                                                    if csc.crank_revs.is_some() {
                                                        last_revs = csc.crank_revs;
                                                        last_time = csc.crank_event_time;
                                                    }
                                                    let Some(rpm) = cadence else { continue };
                                                    let readings = LiveReadings {
                                                        cadence_rpm: Some(rpm),
                                                        ..Default::default()
                                                    };
                                                    if event_tx
                                                        .send(DeviceEvent::Readings(readings))
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("notifications() failed: {e}")
                                            }
                                        }
                                    });
                                    cadence_sensor = Some(peripheral);
                                    cadence_sensor_address = Some(address.clone());
                                    connected_as = DeviceType::CadenceSensor;
                                    tracing::info!("Cadence sensor connected: {address}");
                                }

                            } else {
                                tracing::warn!(
                                    "Connect: {address} has no FTMS, power, HR, or CSC characteristics — ignoring"
                                );
                            }

                            let _ = self.event_tx.send(DeviceEvent::ConnectionChanged {
                                address,
                                connected: true,
                                device_type: Some(connected_as),
                            }).await;
                        }

                        DeviceCommand::Disconnect { address } => {
                            if address.starts_with("ant:") {
                                let _ = ant_tx.send(crate::devices::ant::AntCommand::Disconnect);
                                ant_connected = false;
                                continue;
                            }
                            if trainer_address.as_deref() == Some(address.as_str()) {
                                if let Some(ref p) = trainer {
                                    p.disconnect().await.ok();
                                }
                                trainer = None;
                                trainer_address = None;
                                ctrl_char = None;
                                tracing::info!("Trainer disconnected");
                            } else if power_meter_address.as_deref() == Some(address.as_str()) {
                                if let Some(ref p) = power_meter {
                                    p.disconnect().await.ok();
                                }
                                power_meter = None;
                                power_meter_address = None;
                                tracing::info!("Power meter disconnected");
                            } else if hr_monitor_address.as_deref() == Some(address.as_str()) {
                                if let Some(ref p) = hr_monitor {
                                    p.disconnect().await.ok();
                                }
                                hr_monitor = None;
                                hr_monitor_address = None;
                                tracing::info!("HR monitor disconnected");
                            } else if cadence_sensor_address.as_deref() == Some(address.as_str()) {
                                if let Some(ref p) = cadence_sensor {
                                    p.disconnect().await.ok();
                                }
                                cadence_sensor = None;
                                cadence_sensor_address = None;
                                tracing::info!("Cadence sensor disconnected");
                            }
                            let _ = self.event_tx.send(DeviceEvent::ConnectionChanged {
                                address,
                                connected: false,
                                device_type: None,
                            }).await;
                        }

                        DeviceCommand::SetTargetPower { watts } => {
                            // Forward to the ANT trainer (it applies the target only when
                            // connected and ERG is enabled).
                            let _ = ant_tx.send(
                                crate::devices::ant::AntCommand::SetTargetPower(watts),
                            );
                            if erg_enabled {
                                if let (Some(ref p), Some(ref ch)) = (&trainer, &ctrl_char) {
                                    tracing::debug!("ERG target: {watts}W");
                                    let cmd = set_target_power_command(watts);
                                    if let Err(e) =
                                        p.write(ch, &cmd, WriteType::WithoutResponse).await
                                    {
                                        tracing::warn!("ERG write failed: {e}");
                                    }
                                } else if !ant_connected {
                                    tracing::warn!(
                                        "ERG target {watts}W dropped — no trainer/control point connected"
                                    );
                                }
                            }
                        }

                        DeviceCommand::SetErgMode(enabled) => {
                            erg_enabled = enabled;
                            let _ = ant_tx
                                .send(crate::devices::ant::AntCommand::SetErgMode(enabled));
                            tracing::info!("ERG mode: {}", if enabled { "on" } else { "off" });
                        }
                    }
                }

                // Forward CentralEvents from the spawned scan task.
                Some(ble_event) = ble_rx.recv() => {
                    match ble_event {
                        CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => {
                            let Some(ref adapter) = adapter else { continue; };
                            if let Ok(peripheral) = adapter.peripheral(&id).await {
                                if let Ok(Some(props)) = peripheral.properties().await {
                                    // Only surface devices that advertise a cycling-relevant
                                    // GATT service, and classify them by role so the UI can
                                    // say "Heart rate monitor" instead of a raw device name.
                                    // FTMS wins over CPS/CSC: trainers often advertise all
                                    // three. Devices that haven't included service UUIDs in
                                    // their advertisement packet are also skipped; they will
                                    // reappear via a DeviceUpdated event once the data arrives.
                                    let has_svc = |uuid: &str| {
                                        props.services.iter().any(|svc| svc.to_string() == uuid)
                                    };
                                    let kind = if has_svc(FTMS_SERVICE_UUID) {
                                        DeviceType::FtmsTrainer
                                    } else if has_svc(CYCLING_POWER_SERVICE_UUID) {
                                        DeviceType::CyclingPowerMeter
                                    } else if has_svc(HR_SERVICE_UUID) {
                                        DeviceType::HeartRateMonitor
                                    } else if has_svc(CSC_SERVICE_UUID) {
                                        DeviceType::CadenceSensor
                                    } else {
                                        continue;
                                    };

                                    let name = props.local_name.unwrap_or_else(|| "Unknown".into());
                                    let rssi = props.rssi;
                                    let addr = id.to_string();
                                    discovered.insert(addr.clone(), peripheral);
                                    let _ = self.event_tx.send(DeviceEvent::PeripheralDiscovered {
                                        address: addr,
                                        name,
                                        rssi,
                                        transport: Transport::BluetoothLe,
                                        kind,
                                    }).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }
}
