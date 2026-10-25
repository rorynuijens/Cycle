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
use crate::devices::ant::{AntCommand, AntDevice};
use crate::devices::ftms::{
    parse_csc_measurement, parse_cycling_power_measurement, parse_hr_measurement,
    parse_indoor_bike_data, request_control_command, set_simulation_command,
    set_target_power_command, start_resume_command, CrankCadence, CONTROL_POINT_UUID,
    CSC_MEASUREMENT_UUID, CSC_SERVICE_UUID, CYCLING_POWER_MEASUREMENT_UUID,
    CYCLING_POWER_SERVICE_UUID, FTMS_SERVICE_UUID, HR_MEASUREMENT_UUID, HR_SERVICE_UUID,
    INDOOR_BIKE_DATA_UUID, MAX_ERG_TARGET_W, MAX_PLAUSIBLE_POWER_W,
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
    /// Use only this radio: scan and connect over it alone, and drop every
    /// device on the other.
    SetRadio(Transport),
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
    /// A connect the UI asked for did not happen. Without this the Devices page
    /// waits on "Connecting…" for good, and skips the device on every rescan
    /// because a connect is still in flight.
    ConnectFailed {
        address: String,
    },
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

/// What the ANT thread is told when the rider connects (`connect`) or
/// disconnects one ANT+ device, or `None` when there is nothing to tell it.
///
/// Each device on the stick comes up and down on its own — dropping one must
/// not take the rest of the stick with it.
/// Whether `address` belongs to the radio in use. Every ANT+ address is a
/// synthetic `ant:` one, so anything else is Bluetooth.
fn uses_radio(radio: Transport, address: &str) -> bool {
    let is_ant = crate::devices::ant::classify_address(address).is_some();
    is_ant == (radio == Transport::AntPlus)
}

fn ant_command_for(device: AntDevice, connect: bool) -> Option<AntCommand> {
    match (device, connect) {
        (AntDevice::Trainer, true) => Some(AntCommand::ConnectTrainer),
        (AntDevice::Trainer, false) => Some(AntCommand::DisconnectTrainer),
        // Broadcast-only, so live the moment it is heard; this only matters
        // for a strap the rider disconnected, whose channel has to reopen.
        (AntDevice::HeartRate, true) => Some(AntCommand::ConnectHr),
        (AntDevice::HeartRate, false) => Some(AntCommand::DisconnectHr),
        (AntDevice::Cadence, true) => Some(AntCommand::ConnectCadence),
        (AntDevice::Cadence, false) => Some(AntCommand::DisconnectCadence),
        (AntDevice::Unknown, _) => None,
    }
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
        let mut crank = CrankCadence::default();
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
                            let cadence = match (cpp.crank_revs, cpp.crank_event_time) {
                                (Some(revs), Some(time)) => {
                                    crank.update(revs, time, std::time::Instant::now())
                                }
                                _ => None,
                            };
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
        let mut crank = CrankCadence::default();
        match p.notifications().await {
            Ok(mut stream) => {
                while let Some(n) = stream.next().await {
                    if n.uuid.to_string() != CYCLING_POWER_MEASUREMENT_UUID {
                        continue;
                    }
                    let Some(cpp) = parse_cycling_power_measurement(&n.value) else {
                        continue;
                    };
                    let cadence = match (cpp.crank_revs, cpp.crank_event_time) {
                        (Some(revs), Some(time)) => {
                            crank.update(revs, time, std::time::Instant::now())
                        }
                        _ => None,
                    };
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
        let mut crank = CrankCadence::default();
        match p.notifications().await {
            Ok(mut stream) => {
                while let Some(n) = stream.next().await {
                    if n.uuid.to_string() != CSC_MEASUREMENT_UUID {
                        continue;
                    }
                    let Some(csc) = parse_csc_measurement(&n.value) else {
                        continue;
                    };
                    let cadence = match (csc.crank_revs, csc.crank_event_time) {
                        (Some(revs), Some(time)) => {
                            crank.update(revs, time, std::time::Instant::now())
                        }
                        _ => None,
                    };
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
        // The one radio in use — see `DeviceCommand::SetRadio`. ANT+ until the
        // UI says otherwise, which it does before it asks for anything else.
        let mut radio = Transport::AntPlus;
        let mut disconnect_now: Vec<String> = Vec::new();

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
                            // One radio or the other, never both.
                            if radio == Transport::AntPlus {
                                let _ = ant_tx.send(crate::devices::ant::AntCommand::Scan);
                                continue;
                            }
                            let Some(ref adapter) = adapter else {
                                tracing::warn!("StartScan: no BLE adapter available");
                                continue;
                            };
                            // The CentralEvent forwarder is already running — see
                            // where it is spawned above.
                            match adapter.start_scan(ScanFilter::default()).await {
                                Ok(_) => {
                                    tracing::info!("BLE scan started");
                                    // A sensor this computer is already connected to
                                    // has stopped advertising, so the scan alone never
                                    // finds it — a link left behind by a failed
                                    // connect, or by another app.
                                    for p in adapter.peripherals().await.unwrap_or_default() {
                                        if p.is_connected().await.unwrap_or(false) {
                                            report_if_cycling(p, &mut discovered, &self.event_tx)
                                                .await;
                                        }
                                    }
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
                            if !uses_radio(radio, &address) {
                                // The Devices page should never ask, but a row left
                                // saying "Connecting…" forever would be worse than
                                // the warning.
                                tracing::warn!("Connect: {address} is not on the radio in use");
                                let _ = self.event_tx.send(DeviceEvent::ConnectFailed { address }).await;
                                continue;
                            }
                            // ANT+ devices are handled by the ANT thread, not btleplug.
                            if let Some(device) = crate::devices::ant::classify_address(&address) {
                                match ant_command_for(device, true) {
                                    Some(cmd) => {
                                        let _ = ant_tx.send(cmd);
                                    }
                                    None if device == AntDevice::Unknown => {
                                        tracing::warn!("Connect: unknown ANT+ address {address}");
                                    }
                                    None => {}
                                }
                                if device == AntDevice::Trainer {
                                    ant_connected = true;
                                }
                                continue;
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
                                    let _ = self.event_tx.send(DeviceEvent::ConnectFailed { address }).await;
                                    continue;
                                }
                            };

                            tracing::info!("Connecting to {address}");
                            let mut attempt = 1;
                            let mut result = peripheral.connect().await;
                            while let Err(e) = &result {
                                let link_up = peripheral.is_connected().await.unwrap_or(false);
                                if !retry_connect(attempt, link_up) {
                                    break;
                                }
                                tracing::info!("connect() attempt {attempt}: {e} — link is up, waiting again");
                                attempt += 1;
                                result = peripheral.connect().await;
                            }
                            if let Err(e) = result {
                                tracing::error!("connect() failed: {e}");
                                // A link BlueZ kept stops the sensor advertising,
                                // so no later scan would find it.
                                peripheral.disconnect().await.ok();
                                let _ = self.event_tx.send(DeviceEvent::ConnectFailed { address }).await;
                                continue;
                            }
                            if let Err(e) = peripheral.discover_services().await {
                                tracing::error!("discover_services() failed: {e}");
                                // Connected but unusable: let go, or BlueZ keeps the
                                // link and the next attempt finds it half-open.
                                peripheral.disconnect().await.ok();
                                let _ = self.event_tx.send(DeviceEvent::ConnectFailed { address }).await;
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

                        DeviceCommand::Disconnect { address } => disconnect_now.push(address),

                        DeviceCommand::SetRadio(chosen) => {
                            if chosen == radio {
                                continue;
                            }
                            radio = chosen;
                            tracing::info!("Device radio: {}", radio.label());
                            let _ = ant_tx.send(crate::devices::ant::AntCommand::SetEnabled(
                                radio == Transport::AntPlus,
                            ));
                            // Bluetooth has to let go of its devices itself; the ANT
                            // thread reports its own on the way down.
                            if radio == Transport::AntPlus {
                                if let Some(ref adapter) = adapter {
                                    adapter.stop_scan().await.ok();
                                }
                                disconnect_now.extend(
                                    [
                                        &trainer_address,
                                        &power_meter_address,
                                        &hr_monitor_address,
                                        &cadence_sensor_address,
                                    ]
                                    .into_iter()
                                    .flatten()
                                    .cloned(),
                                );
                            }
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

                    // Disconnects run here, after the match, so a radio switch
                    // drops each Bluetooth device exactly as the rider dropping
                    // it would.
                    for address in std::mem::take(&mut disconnect_now) {
                        // Each ANT+ device comes down on its own — dropping one
                        // must not take the rest of the stick with it.
                        if let Some(device) = crate::devices::ant::classify_address(&address) {
                            match ant_command_for(device, false) {
                                Some(cmd) => {
                                    let _ = ant_tx.send(cmd);
                                }
                                None => {
                                    tracing::warn!("Disconnect: unknown ANT+ address {address}");
                                }
                            }
                            if device == AntDevice::Trainer {
                                ant_connected = false;
                            }
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
                }

                // Forward CentralEvents from the spawned scan task.
                Some(ble_event) = ble_rx.recv() => {
                    match ble_event {
                        CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => {
                            // BlueZ reports devices other programs find too, so
                            // stopping our own scan is not enough to keep
                            // Bluetooth out of the list.
                            if radio != Transport::BluetoothLe {
                                continue;
                            }
                            let Some(ref adapter) = adapter else { continue; };
                            if let Ok(peripheral) = adapter.peripheral(&id).await {
                                report_if_cycling(peripheral, &mut discovered, &self.event_tx)
                                    .await;
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

/// How many times `connect()` is called before a device is given up on.
const CONNECT_ATTEMPTS: u32 = 4;

/// Whether to call `connect()` again after attempt number `attempt` failed.
///
/// bluez-async gives a new link 5 s to list its services, and the Decathlon
/// cadence sensor takes longer. The link stays up through that timeout, so
/// asking again simply waits another 5 s. A link that is down means the
/// device is gone, and waiting would not help.
fn retry_connect(attempt: u32, link_up: bool) -> bool {
    link_up && attempt < CONNECT_ATTEMPTS
}

/// The role a Bluetooth device plays, from the GATT services it lists, or
/// `None` when it offers nothing a ride uses.
///
/// FTMS wins over power and cadence because trainers often list all three.
fn classify_services<'a>(services: impl IntoIterator<Item = &'a str>) -> Option<DeviceType> {
    let services: Vec<&str> = services.into_iter().collect();
    let has = |uuid: &str| services.iter().any(|s| s.eq_ignore_ascii_case(uuid));
    if has(FTMS_SERVICE_UUID) {
        Some(DeviceType::FtmsTrainer)
    } else if has(CYCLING_POWER_SERVICE_UUID) {
        Some(DeviceType::CyclingPowerMeter)
    } else if has(HR_SERVICE_UUID) {
        Some(DeviceType::HeartRateMonitor)
    } else if has(CSC_SERVICE_UUID) {
        Some(DeviceType::CadenceSensor)
    } else {
        None
    }
}

/// Tell the UI about `peripheral` if it is a cycling device, and remember it
/// so a later Connect can find it.
///
/// A device whose advertisement has not carried its services yet is skipped;
/// it comes back as a DeviceUpdated once they arrive.
async fn report_if_cycling(
    peripheral: Peripheral,
    discovered: &mut HashMap<String, Peripheral>,
    event_tx: &Sender<DeviceEvent>,
) {
    let Ok(Some(props)) = peripheral.properties().await else {
        return;
    };
    let services: Vec<String> = props.services.iter().map(|s| s.to_string()).collect();
    let Some(kind) = classify_services(services.iter().map(String::as_str)) else {
        return;
    };
    let address = peripheral.id().to_string();
    discovered.insert(address.clone(), peripheral);
    let _ = event_tx
        .send(DeviceEvent::PeripheralDiscovered {
            address,
            name: props.local_name.unwrap_or_else(|| "Unknown".into()),
            rssi: props.rssi,
            transport: Transport::BluetoothLe,
            kind,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── Bluetooth roles ──────────────────────────────────────────────────────

    #[test]
    fn should_wait_again_only_while_the_link_is_up() {
        assert!(retry_connect(1, true));
        assert!(!retry_connect(1, false));
    }

    #[test]
    fn should_stop_after_the_fourth_attempt() {
        // Four calls of 5 s each: 20 s before the row says it failed.
        assert!(retry_connect(3, true));
        assert!(!retry_connect(4, true));
        assert!(!retry_connect(u32::MAX, true));
    }

    #[test]
    fn should_classify_the_decathlon_sensor_as_cadence() {
        // The services the Decathlon dual-band sensor lists, read from BlueZ.
        let services = [
            "00001800-0000-1000-8000-00805f9b34fb",
            "00001801-0000-1000-8000-00805f9b34fb",
            "0000180a-0000-1000-8000-00805f9b34fb",
            "0000180f-0000-1000-8000-00805f9b34fb",
            "00001816-0000-1000-8000-00805f9b34fb",
            "8ce5cc01-0a4d-11e9-ab14-d663bd873d93",
        ];
        assert_eq!(classify_services(services), Some(DeviceType::CadenceSensor));
    }

    #[test]
    fn should_rank_trainer_over_power_over_hr_over_cadence() {
        use crate::devices::ftms::*;
        let all = [
            CSC_SERVICE_UUID,
            HR_SERVICE_UUID,
            CYCLING_POWER_SERVICE_UUID,
        ];
        assert_eq!(classify_services(all), Some(DeviceType::CyclingPowerMeter));
        let with_ftms = [
            CSC_SERVICE_UUID,
            FTMS_SERVICE_UUID,
            CYCLING_POWER_SERVICE_UUID,
        ];
        assert_eq!(classify_services(with_ftms), Some(DeviceType::FtmsTrainer));
        let hr_and_csc = [CSC_SERVICE_UUID, HR_SERVICE_UUID];
        assert_eq!(
            classify_services(hr_and_csc),
            Some(DeviceType::HeartRateMonitor)
        );
    }

    #[test]
    fn should_ignore_devices_with_no_ride_service() {
        // A mouse: generic access, device info, battery, HID.
        let mouse = [
            "00001800-0000-1000-8000-00805f9b34fb",
            "0000180a-0000-1000-8000-00805f9b34fb",
            "0000180f-0000-1000-8000-00805f9b34fb",
            "00001812-0000-1000-8000-00805f9b34fb",
        ];
        assert_eq!(classify_services(mouse), None);
        assert_eq!(classify_services([]), None);
        // One digit off the cadence service is not the cadence service.
        assert_eq!(
            classify_services(["00001817-0000-1000-8000-00805f9b34fb"]),
            None
        );
    }

    #[test]
    fn should_match_service_ids_in_upper_case() {
        assert_eq!(
            classify_services(["00001816-0000-1000-8000-00805F9B34FB"]),
            Some(DeviceType::CadenceSensor)
        );
    }

    // ── ERG target ceiling (CLAUDE.md §5.1) ──────────────────────────────────

    #[test]
    fn should_connect_only_devices_on_the_radio_in_use() {
        let ble = "hci0/dev_C0_B8_1C_6C_B5_CF";
        use crate::devices::ant::{ANT_CADENCE_ADDRESS, ANT_FEC_ADDRESS, ANT_HR_ADDRESS};
        for ant in [
            ANT_FEC_ADDRESS,
            ANT_HR_ADDRESS,
            ANT_CADENCE_ADDRESS,
            "ant:power",
        ] {
            assert!(uses_radio(Transport::AntPlus, ant), "{ant}");
            assert!(!uses_radio(Transport::BluetoothLe, ant), "{ant}");
        }
        assert!(uses_radio(Transport::BluetoothLe, ble));
        assert!(!uses_radio(Transport::AntPlus, ble));
    }

    #[test]
    fn should_send_each_ant_device_its_own_connect() {
        assert_eq!(
            ant_command_for(AntDevice::Trainer, true),
            Some(AntCommand::ConnectTrainer)
        );
        assert_eq!(
            ant_command_for(AntDevice::HeartRate, true),
            Some(AntCommand::ConnectHr)
        );
        assert_eq!(
            ant_command_for(AntDevice::Cadence, true),
            Some(AntCommand::ConnectCadence)
        );
        assert_eq!(ant_command_for(AntDevice::Unknown, true), None);
    }

    #[test]
    fn should_send_each_ant_device_its_own_disconnect() {
        assert_eq!(
            ant_command_for(AntDevice::Trainer, false),
            Some(AntCommand::DisconnectTrainer)
        );
        assert_eq!(
            ant_command_for(AntDevice::HeartRate, false),
            Some(AntCommand::DisconnectHr)
        );
        assert_eq!(
            ant_command_for(AntDevice::Cadence, false),
            Some(AntCommand::DisconnectCadence)
        );
        assert_eq!(ant_command_for(AntDevice::Unknown, false), None);
    }

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
