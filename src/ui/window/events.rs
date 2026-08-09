//! The bridge from the device thread to the widgets.
//!
//! `DeviceManager` runs on tokio and pushes events down an async channel; this
//! polls that channel on the GLib loop, which is the only place a widget may be
//! touched (CLAUDE.md §2.3).

use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use async_channel::Receiver;

use crate::devices::manager::{DeviceEvent, DeviceType};
use crate::ui::pages::{devices::DevicesPage, player::PlayerPage, route_player::RoutePlayerPage};

/// How often the device channel is drained. Fast enough that a power reading
/// lands within a frame, cheap enough to leave the loop idle between rides.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Start draining `event_rx` onto the widgets, for the life of the window.
#[allow(clippy::too_many_arguments)]
pub fn start_polling(
    event_rx: Receiver<DeviceEvent>,
    player_for_loop: Rc<RefCell<PlayerPage>>,
    route_player_for_loop: Rc<RoutePlayerPage>,
    devices_for_loop: Rc<RefCell<DevicesPage>>,
    toast_overlay_for_loop: adw::ToastOverlay,
    trainer_addr: Rc<RefCell<Option<String>>>,
    sim_capable: Rc<Cell<bool>>,
) {
    // Last connection state announced per address. A device driver that repeats
    // itself must not be able to toast the rider twice, or — worse — drive the
    // Devices page into restarting a scan on every repeat.
    let mut last_connection: HashMap<String, bool> = HashMap::new();

    glib::timeout_add_local(POLL_INTERVAL, move || {
        while let Ok(event) = event_rx.try_recv() {
            match event {
                DeviceEvent::Readings(readings) => {
                    player_for_loop.borrow().set_readings(readings.clone());
                    route_player_for_loop.set_readings(readings);
                }
                DeviceEvent::PeripheralDiscovered {
                    address,
                    name,
                    rssi,
                    transport,
                    kind,
                } => {
                    devices_for_loop
                        .borrow_mut()
                        .on_discovered(address, name, rssi, transport, kind);
                }
                DeviceEvent::ConnectionChanged {
                    address,
                    connected,
                    device_type,
                } => {
                    if last_connection.insert(address.clone(), connected) == Some(connected) {
                        continue; // nothing changed — no toast, no rescan
                    }
                    let display_name = devices_for_loop.borrow().display_name_for(&address);
                    devices_for_loop.borrow_mut().on_connection_changed(
                        address.clone(),
                        connected,
                        device_type,
                    );
                    // Track whether a controllable trainer is available for SIM mode.
                    if connected && device_type == Some(DeviceType::FtmsTrainer) {
                        *trainer_addr.borrow_mut() = Some(address.clone());
                        sim_capable.set(true);
                    } else if !connected
                        && trainer_addr.borrow().as_deref() == Some(address.as_str())
                    {
                        *trainer_addr.borrow_mut() = None;
                        sim_capable.set(false);
                    }
                    if connected {
                        player_for_loop
                            .borrow()
                            .add_connected_device(&address, &display_name);
                        route_player_for_loop.add_connected_device(&address, &display_name);
                        toast_overlay_for_loop.add_toast(
                            adw::Toast::builder()
                                .title(format!("Connected: {}", display_name))
                                .timeout(4)
                                .build(),
                        );
                    } else {
                        player_for_loop.borrow().remove_connected_device(&address);
                        route_player_for_loop.remove_connected_device(&address);
                        // Losing a device matters more mid-ride than gaining one,
                        // so it gets the longer of the two timeouts. Without this
                        // a dropout was announced only by a chip disappearing.
                        toast_overlay_for_loop.add_toast(
                            adw::Toast::builder()
                                .title(format!("Disconnected: {}", display_name))
                                .timeout(6)
                                .build(),
                        );
                    }
                }
                DeviceEvent::Error(e) => {
                    tracing::error!("Device error: {e}");
                }
                DeviceEvent::Warning(msg) => {
                    tracing::warn!("Device warning: {msg}");
                    toast_overlay_for_loop
                        .add_toast(adw::Toast::builder().title(msg).timeout(5).build());
                }
            }
        }
        glib::ControlFlow::Continue
    });
}
