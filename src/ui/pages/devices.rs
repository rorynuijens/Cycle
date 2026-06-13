use adw::prelude::*;
use async_channel::Sender;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use sqlx::SqlitePool;

use crate::data::db::{self, SavedDevice};
use crate::devices::manager::{DeviceCommand, DeviceType};
use crate::devices::peripheral::Transport;

#[derive(Clone, Copy, PartialEq)]
enum DeviceLocation {
    Known,
    Nearby,
    Connected,
}

struct DeviceEntry {
    ble_name: String,
    display_name: String,
    rssi: Option<i16>,
    transport: Transport,
    location: DeviceLocation,
    row: adw::ActionRow,
    erg_enabled: bool,
}

pub struct DevicesPage {
    root: gtk::Box,
    cmd_tx: Sender<DeviceCommand>,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    known_group: adw::PreferencesGroup,
    connected_group: adw::PreferencesGroup,
    nearby_group: adw::PreferencesGroup,
    scan_btn: gtk::Button,
    scan_spinner: gtk::Spinner,
    entries: Rc<RefCell<HashMap<String, DeviceEntry>>>,
    auto_reconnect: Rc<Cell<bool>>,
}

impl DevicesPage {
    pub fn new(
        cmd_tx: Sender<DeviceCommand>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        saved_devices: Vec<SavedDevice>,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let prefs_page = adw::PreferencesPage::new();

        // ── Previously Connected ─────────────────────────────────────────────
        let known_group = adw::PreferencesGroup::builder()
            .title("Previously Connected")
            .visible(!saved_devices.is_empty())
            .build();
        prefs_page.add(&known_group);

        // ── Connected ────────────────────────────────────────────────────────
        let connected_group = adw::PreferencesGroup::builder().title("Connected").build();
        prefs_page.add(&connected_group);

        // ── Nearby ───────────────────────────────────────────────────────────
        let nearby_group = adw::PreferencesGroup::builder()
            .title("Nearby")
            .description("Tap Connect to connect a device")
            .build();

        let scan_controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        let scan_spinner = gtk::Spinner::builder().visible(false).build();
        let scan_btn = gtk::Button::builder()
            .label("Scan")
            .css_classes(["pill"])
            .tooltip_text("Scan for nearby Bluetooth devices")
            .build();
        scan_controls.append(&scan_spinner);
        scan_controls.append(&scan_btn);
        nearby_group.set_header_suffix(Some(&scan_controls));
        prefs_page.add(&nearby_group);

        // ── Settings ─────────────────────────────────────────────────────────
        let auto_reconnect = Rc::new(Cell::new(true));
        let settings_group = adw::PreferencesGroup::builder().title("Settings").build();
        let auto_switch = adw::SwitchRow::builder()
            .title("Auto-reconnect to saved devices")
            .subtitle("Connect automatically when a saved device is detected during a scan")
            .active(true)
            .build();
        let auto_reconnect_switch = Rc::clone(&auto_reconnect);
        auto_switch.connect_active_notify(move |row| {
            auto_reconnect_switch.set(row.is_active());
        });
        settings_group.add(&auto_switch);
        prefs_page.add(&settings_group);

        scroll.set_child(Some(&prefs_page));
        root.append(&scroll);

        let cmd_tx_clone = cmd_tx.clone();
        let spinner_clone = scan_spinner.clone();
        let btn_clone = scan_btn.clone();
        scan_btn.connect_clicked(move |_| {
            let _ = cmd_tx_clone.try_send(DeviceCommand::StartScan);
            spinner_clone.set_visible(true);
            spinner_clone.start();
            btn_clone.set_sensitive(false);
            btn_clone.set_label("Scanning…");
        });

        // Auto-start a scan on launch when saved devices exist so they reconnect
        // without the user having to navigate to the Devices page first.
        if !saved_devices.is_empty() {
            let _ = cmd_tx.try_send(DeviceCommand::StartScan);
            scan_spinner.set_visible(true);
            scan_spinner.start();
            scan_btn.set_sensitive(false);
            scan_btn.set_label("Scanning…");
        }

        let entries: Rc<RefCell<HashMap<String, DeviceEntry>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Populate known group from saved devices
        for saved in &saved_devices {
            let transport = Transport::from_db_str(&saved.transport);
            let row = Self::make_known_row(
                &saved.address,
                &saved.display_name,
                transport,
                pool.clone(),
                rt_handle.clone(),
                Rc::clone(&entries),
                known_group.clone(),
            );
            known_group.add(&row);
            entries.borrow_mut().insert(
                saved.address.clone(),
                DeviceEntry {
                    ble_name: saved.display_name.clone(),
                    display_name: saved.display_name.clone(),
                    rssi: None,
                    transport,
                    location: DeviceLocation::Known,
                    row,
                    erg_enabled: saved.erg_enabled,
                },
            );
        }

        Self {
            root,
            cmd_tx,
            pool,
            rt_handle,
            known_group,
            connected_group,
            nearby_group,
            scan_btn,
            scan_spinner,
            entries,
            auto_reconnect,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn on_discovered(
        &mut self,
        address: String,
        name: String,
        rssi: Option<i16>,
        transport: Transport,
    ) {
        let existing_location = {
            let mut entries = self.entries.borrow_mut();
            if let Some(entry) = entries.get_mut(&address) {
                match entry.location {
                    DeviceLocation::Known => {
                        entry.ble_name = name.clone();
                        entry.rssi = rssi;
                        entry.transport = transport;
                        Some(DeviceLocation::Known)
                    }
                    DeviceLocation::Nearby => {
                        entry.rssi = rssi;
                        entry
                            .row
                            .set_subtitle(&Self::nearby_subtitle(&address, rssi));
                        return;
                    }
                    DeviceLocation::Connected => return,
                }
            } else {
                None
            }
        };

        match existing_location {
            Some(DeviceLocation::Known) => {
                // Promote from Known → Nearby
                let (old_row, display_name) = {
                    let entries = self.entries.borrow();
                    let entry = entries
                        .get(&address)
                        .expect("invariant: address present — checked via existing_location above");
                    (entry.row.clone(), entry.display_name.clone())
                };
                self.known_group.remove(&old_row);

                let new_row = self.make_nearby_row(&address, &display_name, transport, rssi);
                self.nearby_group.add(&new_row);

                {
                    let mut entries = self.entries.borrow_mut();
                    let entry = entries.get_mut(&address).expect(
                        "invariant: address present — same map, no removal between borrows",
                    );
                    entry.row = new_row;
                    entry.location = DeviceLocation::Nearby;
                    let has_known = entries
                        .values()
                        .any(|e| e.location == DeviceLocation::Known);
                    self.known_group.set_visible(has_known);
                }

                if self.auto_reconnect.get() {
                    let _ = self.cmd_tx.try_send(DeviceCommand::StopScan);
                    self.scan_spinner.stop();
                    self.scan_spinner.set_visible(false);
                    self.scan_btn.set_sensitive(true);
                    self.scan_btn.set_label("Scan");
                    let _ = self.cmd_tx.try_send(DeviceCommand::Connect {
                        address: address.clone(),
                    });
                }
            }
            None => {
                let row = self.make_nearby_row(&address, &name, transport, rssi);
                self.nearby_group.add(&row);
                self.entries.borrow_mut().insert(
                    address.clone(),
                    DeviceEntry {
                        ble_name: name.clone(),
                        display_name: name,
                        rssi,
                        transport,
                        location: DeviceLocation::Nearby,
                        row,
                        erg_enabled: true,
                    },
                );
            }
            _ => {}
        }
    }

    pub fn on_connection_changed(
        &mut self,
        address: String,
        connected: bool,
        device_type: Option<DeviceType>,
    ) {
        let target = if connected {
            DeviceLocation::Connected
        } else {
            DeviceLocation::Nearby
        };

        let data = {
            let mut entries = self.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&address) else {
                return;
            };
            if entry.location == target {
                return;
            }
            let old_row = entry.row.clone();
            let old_location = entry.location;
            let display_name = entry.display_name.clone();
            let transport = entry.transport;
            let rssi = entry.rssi;
            let erg = entry.erg_enabled;
            entry.location = target;
            (old_row, old_location, display_name, transport, rssi, erg)
        };
        let (old_row, old_location, display_name, transport, rssi, erg_initial) = data;

        let is_trainer = device_type == Some(DeviceType::FtmsTrainer);
        let new_row = if connected {
            self.make_connected_row(&address, &display_name, transport, is_trainer, erg_initial)
        } else {
            self.make_nearby_row(&address, &display_name, transport, rssi)
        };

        if let Some(entry) = self.entries.borrow_mut().get_mut(&address) {
            entry.row = new_row.clone();
        }

        match old_location {
            DeviceLocation::Known => self.known_group.remove(&old_row),
            DeviceLocation::Nearby => self.nearby_group.remove(&old_row),
            DeviceLocation::Connected => self.connected_group.remove(&old_row),
        }

        if connected {
            self.connected_group.add(&new_row);
            self.scan_spinner.stop();
            self.scan_spinner.set_visible(false);
            self.scan_btn.set_sensitive(true);
            self.scan_btn.set_label("Scan");

            let pool = self.pool.clone();
            let addr = address.clone();
            let transport_str = transport.as_db_str().to_string();
            self.rt_handle.spawn(async move {
                if let Err(e) = db::save_device(&pool, &addr, &display_name, &transport_str).await {
                    tracing::error!("save_device failed: {e}");
                }
            });

            // Sync the persisted ERG preference to the manager for FTMS trainers.
            if is_trainer {
                let _ = self.cmd_tx.try_send(DeviceCommand::SetErgMode(erg_initial));
            }
        } else {
            self.nearby_group.add(&new_row);
        }
    }

    /// Return the human-readable display name for a device address, falling back to the address.
    pub fn display_name_for(&self, address: &str) -> String {
        self.entries
            .borrow()
            .get(address)
            .map(|e| e.display_name.clone())
            .unwrap_or_else(|| address.to_string())
    }

    // ── Row constructors ─────────────────────────────────────────────────────

    fn make_known_row(
        address: &str,
        display_name: &str,
        transport: Transport,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        entries: Rc<RefCell<HashMap<String, DeviceEntry>>>,
        known_group: adw::PreferencesGroup,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(display_name)
            .subtitle(address)
            .build();
        row.add_prefix(&Self::transport_badge(transport));

        let forget_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Forget this device")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();

        let addr = address.to_string();
        let row_clone = row.clone();
        forget_btn.connect_clicked(move |btn| {
            let dialog = adw::AlertDialog::builder()
                .heading("Forget Device?")
                .body("This device will be removed from your saved devices.")
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("forget", "_Forget");
            dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");

            let addr = addr.clone();
            let entries = Rc::clone(&entries);
            let group = known_group.clone();
            let pool = pool.clone();
            let rt = rt_handle.clone();
            let rw = row_clone.clone();

            dialog.connect_response(None, move |_, resp| {
                if resp != "forget" {
                    return;
                }
                entries.borrow_mut().remove(&addr);
                group.remove(&rw);
                let has_known = entries
                    .borrow()
                    .values()
                    .any(|e| e.location == DeviceLocation::Known);
                group.set_visible(has_known);
                let pool = pool.clone();
                let a = addr.clone();
                rt.spawn(async move {
                    if let Err(e) = db::delete_device(&pool, &a).await {
                        tracing::error!("delete_device failed: {e}");
                    }
                });
            });

            dialog.present(Some(btn));
        });
        row.add_suffix(&forget_btn);

        row
    }

    fn make_nearby_row(
        &self,
        address: &str,
        display_name: &str,
        transport: Transport,
        rssi: Option<i16>,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(display_name)
            .subtitle(Self::nearby_subtitle(address, rssi))
            .build();

        row.add_prefix(&Self::transport_badge(transport));

        let connect_btn = gtk::Button::builder()
            .label("Connect")
            .css_classes(["pill"])
            .valign(gtk::Align::Center)
            .tooltip_text("Connect to this device")
            .build();

        let addr = address.to_string();
        let cmd_tx = self.cmd_tx.clone();
        let scan_btn = self.scan_btn.clone();
        let scan_spinner = self.scan_spinner.clone();
        connect_btn.connect_clicked(move |btn| {
            let _ = cmd_tx.try_send(DeviceCommand::StopScan);
            scan_spinner.stop();
            scan_spinner.set_visible(false);
            scan_btn.set_sensitive(true);
            scan_btn.set_label("Scan");
            btn.set_sensitive(false);
            let _ = cmd_tx.try_send(DeviceCommand::Connect {
                address: addr.clone(),
            });
        });

        row.add_suffix(&connect_btn);
        row
    }

    fn make_connected_row(
        &self,
        address: &str,
        display_name: &str,
        transport: Transport,
        show_erg: bool,
        erg_initial: bool,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(display_name)
            .subtitle("Connected")
            .build();

        row.add_prefix(&Self::transport_badge(transport));
        row.add_prefix(
            &gtk::Image::builder()
                .icon_name("object-select-symbolic")
                .css_classes(["success"])
                .valign(gtk::Align::Center)
                .build(),
        );

        // ERG toggle (FTMS trainers only)
        if show_erg {
            let erg_sep = gtk::Separator::builder()
                .orientation(gtk::Orientation::Vertical)
                .margin_top(8)
                .margin_bottom(8)
                .build();
            let erg_label = gtk::Label::builder()
                .label("ERG")
                .css_classes(["caption", "dim-label"])
                .valign(gtk::Align::Center)
                .build();
            let erg_switch = gtk::Switch::builder()
                .active(erg_initial)
                .valign(gtk::Align::Center)
                .tooltip_text(
                    "When on, the trainer automatically adjusts resistance to match target power",
                )
                .build();

            let cmd_tx_erg = self.cmd_tx.clone();
            let pool_erg = self.pool.clone();
            let rt_erg = self.rt_handle.clone();
            let addr_erg = address.to_string();
            let entries_erg = Rc::clone(&self.entries);
            erg_switch.connect_active_notify(move |sw| {
                let enabled = sw.is_active();
                if let Some(e) = entries_erg.borrow_mut().get_mut(&addr_erg) {
                    e.erg_enabled = enabled;
                }
                let _ = cmd_tx_erg.try_send(DeviceCommand::SetErgMode(enabled));
                let pool = pool_erg.clone();
                let a = addr_erg.clone();
                rt_erg.spawn(async move {
                    if let Err(e) = db::set_device_erg_enabled(&pool, &a, enabled).await {
                        tracing::error!("set_device_erg_enabled failed: {e}");
                    }
                });
            });

            row.add_suffix(&erg_sep);
            row.add_suffix(&erg_label);
            row.add_suffix(&erg_switch);
        }

        // Rename button
        let rename_btn = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename this device")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();

        let addr_rename = address.to_string();
        let entries_rc = Rc::clone(&self.entries);
        let pool_rename = self.pool.clone();
        let rt_rename = self.rt_handle.clone();
        let row_clone = row.clone();
        rename_btn.connect_clicked(move |btn| {
            let current_name = entries_rc
                .borrow()
                .get(&addr_rename)
                .map(|e| e.display_name.clone())
                .unwrap_or_default();
            Self::show_rename_dialog(
                btn,
                &addr_rename,
                &current_name,
                Rc::clone(&entries_rc),
                pool_rename.clone(),
                rt_rename.clone(),
                row_clone.clone(),
            );
        });
        row.add_suffix(&rename_btn);

        // Forget button
        let forget_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Forget this device")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();

        let addr_forget = address.to_string();
        let entries_forget = Rc::clone(&self.entries);
        let connected_group = self.connected_group.clone();
        let pool_forget = self.pool.clone();
        let rt_forget = self.rt_handle.clone();
        let cmd_forget = self.cmd_tx.clone();
        let row_forget = row.clone();

        forget_btn.connect_clicked(move |btn| {
            let dialog = adw::AlertDialog::builder()
                .heading("Forget Device?")
                .body("This device will be removed from your saved devices.")
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("forget", "_Forget");
            dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");

            let addr = addr_forget.clone();
            let entries = Rc::clone(&entries_forget);
            let grp = connected_group.clone();
            let pool = pool_forget.clone();
            let rt = rt_forget.clone();
            let cmd = cmd_forget.clone();
            let rw = row_forget.clone();

            dialog.connect_response(None, move |_, resp| {
                if resp != "forget" {
                    return;
                }
                entries.borrow_mut().remove(&addr);
                grp.remove(&rw);
                // Sending Disconnect: on_connection_changed will be a no-op since
                // the entry was already removed from the map.
                let _ = cmd.try_send(DeviceCommand::Disconnect {
                    address: addr.clone(),
                });
                let pool = pool.clone();
                let a = addr.clone();
                rt.spawn(async move {
                    if let Err(e) = db::delete_device(&pool, &a).await {
                        tracing::error!("delete_device failed: {e}");
                    }
                });
            });

            dialog.present(Some(btn));
        });
        row.add_suffix(&forget_btn);

        // Disconnect button
        let disconnect_btn = gtk::Button::builder()
            .label("Disconnect")
            .css_classes(["destructive-action", "pill"])
            .valign(gtk::Align::Center)
            .tooltip_text("Disconnect this device")
            .build();

        let addr = address.to_string();
        let cmd_tx = self.cmd_tx.clone();
        disconnect_btn.connect_clicked(move |_| {
            let _ = cmd_tx.try_send(DeviceCommand::Disconnect {
                address: addr.clone(),
            });
        });
        row.add_suffix(&disconnect_btn);

        row
    }

    fn show_rename_dialog(
        parent: &gtk::Button,
        address: &str,
        current_name: &str,
        entries: Rc<RefCell<HashMap<String, DeviceEntry>>>,
        pool: SqlitePool,
        rt_handle: tokio::runtime::Handle,
        row: adw::ActionRow,
    ) {
        let dialog = adw::AlertDialog::builder()
            .heading("Rename Device")
            .body("Enter a custom name for this device.")
            .build();

        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("rename", "_Rename");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");

        let entry = gtk::Entry::builder()
            .text(current_name)
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));

        let address = address.to_string();
        let entry_widget = entry.clone();
        dialog.connect_response(None, move |_d, response| {
            if response != "rename" {
                return;
            }
            let new_name = entry_widget.text().to_string();
            if new_name.is_empty() {
                return;
            }
            row.set_title(&new_name);
            if let Some(e) = entries.borrow_mut().get_mut(&address) {
                e.display_name = new_name.clone();
            }
            let pool = pool.clone();
            let addr = address.clone();
            rt_handle.spawn(async move {
                if let Err(e) = db::rename_device(&pool, &addr, &new_name).await {
                    tracing::error!("rename_device failed: {e}");
                }
            });
        });

        dialog.present(Some(parent));
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn transport_badge(transport: Transport) -> gtk::Image {
        let img = gtk::Image::builder()
            .icon_name(transport.icon_name())
            .valign(gtk::Align::Center)
            .build();
        img.add_css_class("dim-label");
        img
    }

    fn nearby_subtitle(address: &str, rssi: Option<i16>) -> String {
        match rssi {
            Some(r) => format!("{address} · {r} dBm"),
            None => address.to_string(),
        }
    }
}
