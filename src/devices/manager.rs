use anyhow::Result;
use async_channel::{Receiver, Sender};
use std::collections::HashMap;
use tokio::task::JoinHandle;

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as BtleManager, Peripheral as BtlePeriph,
    ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;

use crate::data::session::{LiveReadings, ReadingSource};
use crate::devices::ant::AntDevice;
use crate::devices::ftms::{
    compute_cadence_rpm, parse_csc_measurement, parse_cycling_power_measurement,
    parse_hr_measurement, parse_indoor_bike_data, request_control_command, set_simulation_command,
    set_target_power_command, start_resume_command, CONTROL_POINT_UUID, CSC_MEASUREMENT_UUID,
    CSC_SERVICE_UUID, CYCLING_POWER_MEASUREMENT_UUID, CYCLING_POWER_SERVICE_UUID,
    FTMS_SERVICE_UUID, HR_MEASUREMENT_UUID, HR_SERVICE_UUID, INDOOR_BIKE_DATA_UUID,
    MAX_ERG_TARGET_W, MAX_PLAUSIBLE_POWER_W,
};
use crate::devices::peripheral::Transport;

/// Commands sent from the UI thread to the Device Manager.
#[derive(Debug)]
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
    /// SIM mode: set the simulated road gradient in percent. The trainer adjusts
    /// resistance to the grade and the rider's power determines virtual speed.
    SetSimulation {
        grade_percent: f32,
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

    /// Symbolic icon representing the device's role. All non-standard names
    /// are bundled in the app gresource (see data/icons/symbolic/README.md),
    /// so they resolve on every icon theme.
    pub fn icon_name(&self) -> &'static str {
        match self {
            Self::FtmsTrainer => "activity-cycling-indoor-symbolic",
            Self::CyclingPowerMeter => "speedometer-symbolic",
            Self::HeartRateMonitor => "heart-symbolic",
            Self::CadenceSensor => "rotate-cw-symbolic",
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

/// The ERG target actually sent to a trainer, whatever was asked for.
///
/// A trainer holds whatever resistance it is told to, so an out-of-range target
/// is not a display bug — it is a rider pushing against a wall mid-interval.
fn erg_target(watts: u16) -> u16 {
    watts.min(MAX_ERG_TARGET_W)
}

/// The notification readers currently running, one per connected address.
///
/// Each reader owns a `Peripheral` clone and forwards every packet it decodes to
/// the UI, so two readers on one device would double every reading the recorder
/// sees. Holding the handles is what makes "one reader per address" enforceable
/// in the task that owns them, instead of resting on the UI never sending a
/// second `Connect` — which it currently avoids only by tracking connection
/// state of its own.
#[derive(Default)]
struct Readers(HashMap<String, JoinHandle<()>>);

impl Readers {
    /// Register the reader for `address`, stopping whatever was reading it.
    ///
    /// Replacing rather than refusing keeps a reconnect able to heal a
    /// half-dead connection: the rider pressing Connect again gets a working
    /// device, not a second stream onto the same one.
    fn insert(&mut self, address: &str, handle: JoinHandle<()>) {
        if let Some(previous) = self.0.insert(address.to_string(), handle) {
            tracing::debug!("{address}: replacing a notification reader that was still running");
            previous.abort();
        }
    }

    /// Stop the reader for `address`, if one is running.
    ///
    /// A reader normally ends on its own when the peripheral disconnects and the
    /// stream closes. This is for when it does not: without it the task, its
    /// `Peripheral` and its channel handle stay alive for the life of the
    /// process, and nothing here would ever notice.
    fn remove(&mut self, address: &str) {
        if let Some(handle) = self.0.remove(address) {
            handle.abort();
        }
    }

    /// Stop every reader — the manager itself is going away.
    fn shutdown(&mut self) {
        for (_, handle) in self.0.drain() {
            handle.abort();
        }
    }
}

/// Read an FTMS trainer: Indoor Bike Data, plus the Cycling Power and Heart
/// Rate characteristics some trainers also expose on the same connection.
fn spawn_trainer_reader(p: Peripheral, event_tx: Sender<DeviceEvent>) -> JoinHandle<()> {
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
                                power_watts: data.power_watts,
                                heart_rate_bpm: data.heart_rate_bpm,
                                cadence_rpm: data.cadence_rpm,
                                speed_kmh: data.speed_kmh,
                                resistance_target_watts: None,
                                source: ReadingSource::Ble,
                            };
                            if event_tx
                                .send(DeviceEvent::Readings(readings))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    } else if uuid == CYCLING_POWER_MEASUREMENT_UUID {
                        if let Some(cpp) = parse_cycling_power_measurement(&n.value) {
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
                                    cpp.power_watts.clamp(0, MAX_PLAUSIBLE_POWER_W as i32) as u32,
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
                    } else if uuid == HR_MEASUREMENT_UUID {
                        if let Some(hr) = parse_hr_measurement(&n.value) {
                            let readings = LiveReadings {
                                heart_rate_bpm: Some(hr),
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
                }
            }
            Err(e) => tracing::error!("notifications() failed: {e}"),
        }
    })
}

/// Read a standalone power meter's Cycling Power Measurement, deriving cadence
/// from its crank counters.
fn spawn_power_meter_reader(p: Peripheral, event_tx: Sender<DeviceEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_revs: Option<u16> = None;
        let mut last_time: Option<u16> = None;
        match p.notifications().await {
            Ok(mut stream) => {
                while let Some(n) = stream.next().await {
                    if n.uuid.to_string() != CYCLING_POWER_MEASUREMENT_UUID {
                        continue;
                    }
                    let Some(cpp) = parse_cycling_power_measurement(&n.value) else {
                        continue;
                    };
                    let cadence = match (last_revs, last_time, cpp.crank_revs, cpp.crank_event_time)
                    {
                        (Some(pr), Some(pt), Some(cr), Some(ct)) => {
                            compute_cadence_rpm(pr, pt, cr, ct)
                        }
                        _ => None,
                    };
                    last_revs = cpp.crank_revs;
                    last_time = cpp.crank_event_time;
                    let readings = LiveReadings {
                        power_watts: Some(
                            cpp.power_watts.clamp(0, MAX_PLAUSIBLE_POWER_W as i32) as u32
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
    })
}

/// Read a dedicated heart rate monitor.
///
/// Unlike the others this reports the stream ending: BlueZ raises no error when
/// the strap goes away, and an unreported silence looks exactly like a rider
/// with a very steady heart rate.
fn spawn_hr_reader(
    p: Peripheral,
    event_tx: Sender<DeviceEvent>,
    stream_address: String,
) -> JoinHandle<()> {
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
                        if event_tx
                            .send(DeviceEvent::Readings(readings))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                // The stream ending means the peripheral went away —
                // BlueZ reports no error for this. Left unreported it
                // looks exactly like a rider with a very steady heart
                // rate, so say so and mark the device disconnected.
                tracing::warn!(
                    "HR notification stream ended for {stream_address} — monitor stopped reporting"
                );
                let _ = event_tx
                    .send(DeviceEvent::ConnectionChanged {
                        address: stream_address,
                        connected: false,
                        device_type: None,
                    })
                    .await;
            }
            Err(e) => tracing::error!("notifications() failed: {e}"),
        }
    })
}

/// Read a dedicated CSC cadence sensor, deriving rpm from successive crank
/// samples (the counters are cumulative and wrap — see ftms.rs).
fn spawn_cadence_reader(p: Peripheral, event_tx: Sender<DeviceEvent>) -> JoinHandle<()> {
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
                    let Some(csc) = parse_csc_measurement(&n.value) else {
                        continue;
                    };
                    let cadence = match (last_revs, last_time, csc.crank_revs, csc.crank_event_time)
                    {
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
    })
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

        // One subscription for the life of the manager, not one per scan.
        // `adapter.events()` is a broadcast that is independent of scan state —
        // it simply goes quiet between scans — so subscribing per StartScan left
        // a task and a stream behind on every scan, and delivered every later
        // advertisement once per scan ever started. Auto-reconnect runs up to
        // five scans per dropout, so that multiplied fast during exactly the
        // ride where the radio was already struggling.
        if let Some(ref adapter) = adapter {
            let adapter = adapter.clone();
            let tx = ble_tx.clone();
            tokio::spawn(async move {
                match adapter.events().await {
                    Ok(mut stream) => {
                        while let Some(ev) = stream.next().await {
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => tracing::error!("BLE event stream: {e}"),
                }
            });
        }

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
        // One notification reader per connected address — see [`Readers`].
        let mut readers = Readers::default();

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
                        readers.shutdown();
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
                            // The CentralEvent forwarder is already running — see
                            // where it is spawned above.
                            match adapter.start_scan(ScanFilter::default()).await {
                                Ok(_) => tracing::info!("BLE scan started"),
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
                            match crate::devices::ant::classify_address(&address) {
                                Some(AntDevice::Trainer) => {
                                    let _ = ant_tx
                                        .send(crate::devices::ant::AntCommand::ConnectTrainer);
                                    ant_connected = true;
                                    continue;
                                }
                                Some(AntDevice::HeartRate) => {
                                    // Broadcast-only: live the moment it is heard, and
                                    // the ANT thread has already said so.
                                    tracing::debug!("Connect: {address} is broadcast-only");
                                    continue;
                                }
                                Some(AntDevice::Unknown) => {
                                    tracing::warn!("Connect: unknown ANT+ address {address}");
                                    continue;
                                }
                                None => {}
                            }
                            // A reconnect re-subscribes and starts a fresh reader, so
                            // whatever was reading this address has to stop first or both
                            // will report every packet.
                            readers.remove(&address);
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
                                let reader = spawn_trainer_reader(p, event_tx);
                                readers.insert(&address, reader);

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
                                    let reader = spawn_power_meter_reader(p, event_tx);
                                    readers.insert(&address, reader);
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
                                    let stream_address = address.clone();
                                    let reader = spawn_hr_reader(p, event_tx, stream_address);
                                    readers.insert(&address, reader);
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
                                    let reader = spawn_cadence_reader(p, event_tx);
                                    readers.insert(&address, reader);
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
                            // Each ANT+ device comes down on its own — dropping one
                            // must not take the rest of the stick with it.
                            match crate::devices::ant::classify_address(&address) {
                                Some(AntDevice::Trainer) => {
                                    let _ = ant_tx
                                        .send(crate::devices::ant::AntCommand::DisconnectTrainer);
                                    ant_connected = false;
                                    continue;
                                }
                                Some(AntDevice::HeartRate) => {
                                    let _ =
                                        ant_tx.send(crate::devices::ant::AntCommand::DisconnectHr);
                                    continue;
                                }
                                Some(AntDevice::Unknown) => {
                                    tracing::warn!("Disconnect: unknown ANT+ address {address}");
                                    continue;
                                }
                                None => {}
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
                            // Unconditional: the reader has to go whether or not the
                            // address matched one of the roles above, since a device
                            // that failed to bind to any of them still left one running.
                            readers.remove(&address);
                            let _ = self.event_tx.send(DeviceEvent::ConnectionChanged {
                                address,
                                connected: false,
                                device_type: None,
                            }).await;
                        }

                        DeviceCommand::SetTargetPower { watts } => {
                            // Clamped here, at the one place that reaches the hardware,
                            // rather than trusting every caller to have done it
                            // (CLAUDE.md §5.1). Both of today's callers do clamp; this is
                            // what stops the next one from being able to forget.
                            let watts = erg_target(watts);
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

                        DeviceCommand::SetSimulation { grade_percent } => {
                            // Forward to the ANT trainer (it applies the grade only when
                            // connected and resistance control is enabled).
                            let _ = ant_tx.send(
                                crate::devices::ant::AntCommand::SetTrackResistance(grade_percent),
                            );
                            // Same kill switch as ERG: erg_enabled is the user's
                            // "automatic resistance" toggle.
                            if erg_enabled {
                                if let (Some(ref p), Some(ref ch)) = (&trainer, &ctrl_char) {
                                    tracing::debug!("SIM grade: {grade_percent:.1}%");
                                    let cmd = set_simulation_command(grade_percent);
                                    if let Err(e) =
                                        p.write(ch, &cmd, WriteType::WithoutResponse).await
                                    {
                                        tracing::warn!("SIM write failed: {e}");
                                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── ERG target ceiling (CLAUDE.md §5.1) ──────────────────────────────────

    #[test]
    fn should_pass_through_a_target_a_rider_could_actually_hold() {
        assert_eq!(erg_target(250), 250);
        assert_eq!(erg_target(0), 0);
    }

    #[test]
    fn should_clamp_a_target_above_the_ceiling() {
        assert_eq!(erg_target(MAX_ERG_TARGET_W), MAX_ERG_TARGET_W);
        assert_eq!(erg_target(MAX_ERG_TARGET_W + 1), MAX_ERG_TARGET_W);
        assert_eq!(erg_target(u16::MAX), MAX_ERG_TARGET_W);
    }

    // ── notification readers ─────────────────────────────────────────────────

    /// A stand-in for a notification reader: runs until aborted. The sleep is
    /// what makes it abortable — a task is only cancelled at an await point.
    fn spawn_reader() -> tokio::task::JoinHandle<()> {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    /// Let the runtime actually carry out the aborts before asserting.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn should_stop_the_previous_reader_when_one_address_is_connected_twice() {
        // The duplicate-reading bug: two readers on one peripheral each forward
        // every packet, so the recorder counts each reading twice.
        let mut readers = Readers::default();
        let first = spawn_reader();
        let superseded = first.abort_handle();
        readers.insert("AA:BB", first);
        readers.insert("AA:BB", spawn_reader());
        settle().await;

        assert_eq!(readers.0.len(), 1, "one address holds one reader");
        assert!(
            superseded.is_finished(),
            "the superseded reader must be stopped"
        );
        assert!(
            !readers.0["AA:BB"].is_finished(),
            "the newest reader must be the one left running"
        );
    }

    #[tokio::test]
    async fn should_leave_other_devices_alone_when_one_reconnects() {
        let mut readers = Readers::default();
        readers.insert("trainer", spawn_reader());
        readers.insert("hr-strap", spawn_reader());

        readers.insert("trainer", spawn_reader());
        settle().await;

        assert_eq!(readers.0.len(), 2);
        assert!(
            !readers.0["hr-strap"].is_finished(),
            "an unrelated sensor must keep reading"
        );
    }

    #[tokio::test]
    async fn should_stop_a_reader_that_outlives_its_disconnect() {
        // A reader normally ends when its stream closes. This is the case where
        // it does not, and would otherwise run for the life of the process.
        let mut readers = Readers::default();
        readers.insert("AA:BB", spawn_reader());
        let running = readers.0["AA:BB"].abort_handle();

        readers.remove("AA:BB");
        settle().await;

        assert!(readers.0.is_empty());
        assert!(running.is_finished(), "the reader must actually be stopped");
    }

    #[tokio::test]
    async fn should_ignore_a_disconnect_for_an_address_with_no_reader() {
        let mut readers = Readers::default();
        readers.remove("never-connected"); // must not panic
        assert!(readers.0.is_empty());
    }

    #[tokio::test]
    async fn should_stop_every_reader_when_the_manager_shuts_down() {
        let mut readers = Readers::default();
        let mut watched = Vec::new();
        for address in ["trainer", "hr-strap", "cadence"] {
            readers.insert(address, spawn_reader());
            watched.push(readers.0[address].abort_handle());
        }

        readers.shutdown();
        settle().await;

        assert!(readers.0.is_empty(), "shutdown must forget every reader");
        for handle in watched {
            assert!(handle.is_finished(), "shutdown must stop every reader");
        }
    }
}
