#![allow(dead_code)]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeripheralRole {
    SmartTrainer,
    PowerMeter,
    HeartRate,
    Cadence,
    Speed,
}

impl PeripheralRole {
    pub fn label(&self) -> &'static str {
        match self {
            Self::SmartTrainer => "Smart Trainer",
            Self::PowerMeter => "Power Meter",
            Self::HeartRate => "Heart Rate Monitor",
            Self::Cadence => "Cadence Sensor",
            Self::Speed => "Speed Sensor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    BluetoothLe,
    AntPlus,
}

impl Transport {
    pub fn label(&self) -> &'static str {
        match self {
            Self::BluetoothLe => "Bluetooth LE",
            Self::AntPlus => "ANT+",
        }
    }

    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::BluetoothLe => "ble",
            Self::AntPlus => "ant+",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "ant+" => Self::AntPlus,
            _ => Self::BluetoothLe,
        }
    }

    pub fn icon_name(&self) -> &'static str {
        match self {
            Self::BluetoothLe => "bluetooth-symbolic",
            Self::AntPlus => "network-wireless-symbolic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Scanning,
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Clone)]
pub struct PeripheralInfo {
    pub address: String,
    pub name: String,
    pub role: PeripheralRole,
    pub transport: Transport,
    pub state: ConnectionState,
    pub rssi: Option<i16>,
}

impl PeripheralInfo {
    pub fn signal_bars(&self) -> u8 {
        match self.rssi {
            Some(r) if r >= -55 => 4,
            Some(r) if r >= -67 => 3,
            Some(r) if r >= -80 => 2,
            Some(_) => 1,
            None => 0,
        }
    }
}
