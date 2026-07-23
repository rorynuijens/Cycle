//! Devices page — organised around the question "am I ready to ride?".
//!
//! A hero "Your Setup" strip shows the three sensor roles (trainer, heart
//! rate, cadence) and fills in as devices connect. Saved devices live in one
//! stable "My Devices" list whose rows only change status text — rows never
//! jump between groups. Per-device actions (rename, ERG, forget) live in a
//! detail dialog so list rows stay simple. All copy is plain language: no MAC
//! addresses or dBm outside the detail dialog.

use adw::prelude::*;
use async_channel::Sender;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::data::db::{self, SavedDevice};
use crate::devices::manager::{DeviceCommand, DeviceType};
use crate::devices::peripheral::Transport;

/// How long a scan runs before giving up and telling the user nothing was found.
const SCAN_TIMEOUT: Duration = Duration::from_secs(25);

const ADD_DESC_IDLE: &str = "Power on your device, then scan to find it";
const ADD_DESC_SCANNING: &str = "Searching — make sure the device is on and awake";
const ADD_DESC_NOTHING_FOUND: &str =
    "Nothing found. Check the device is on and awake, then scan again.";

struct DeviceEntry {
    display_name: String,
    rssi: Option<i16>,
    transport: Transport,
    kind: DeviceType,
    saved: bool,
    connected: bool,
    /// A connect request is in flight — suppresses duplicate auto-connects.
    connecting: bool,
    /// Discovered at least once this session, so the manager can connect to it.
    seen: bool,
    erg_enabled: bool,
    row: adw::ActionRow,
    /// Status label on saved rows ("Connected" / "Searching…" / "Not connected").
    status_label: Option<gtk::Label>,
}

/// Widgets of one role card in the "Your Setup" strip.
struct SetupSlot {
    icon: gtk::Image,
    name_label: gtk::Label,
    status_label: gtk::Label,
}

struct SlotSet {
    trainer: SetupSlot,
    heart_rate: SetupSlot,
    cadence: SetupSlot,
}

/// Everything page closures need, cloneable so each callback can own a copy.
#[derive(Clone)]
struct PageCtx {
    cmd_tx: Sender<DeviceCommand>,
    pool: SqlitePool,
    rt: tokio::runtime::Handle,
    entries: Rc<RefCell<HashMap<String, DeviceEntry>>>,
    slots: Rc<SlotSet>,
    my_devices_group: adw::PreferencesGroup,
    add_group: adw::PreferencesGroup,
    scan_btn: gtk::Button,
    scan_spinner: gtk::Spinner,
    scanning: Rc<Cell<bool>>,
    scan_generation: Rc<Cell<u32>>,
    auto_reconnect: Rc<Cell<bool>>,
}

pub struct DevicesPage {
    root: gtk::Box,
    ctx: PageCtx,
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

        let prefs_page = adw::PreferencesPage::new();

        // ── "Your Setup" hero strip ──────────────────────────────────────────
        let (strip_box, slots) = make_setup_strip();
        let strip_group = adw::PreferencesGroup::builder().title("Your Setup").build();
        strip_group.add(&strip_box);
        prefs_page.add(&strip_group);

        // ── My Devices ───────────────────────────────────────────────────────
        let my_devices_group = adw::PreferencesGroup::builder()
            .title("My Devices")
            .visible(!saved_devices.is_empty())
            .build();
        prefs_page.add(&my_devices_group);

        // ── Add a Device ─────────────────────────────────────────────────────
        let add_group = adw::PreferencesGroup::builder()
            .title("Add a Device")
            .description(ADD_DESC_IDLE)
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
            .tooltip_text("Scan for nearby devices")
            .build();
        scan_controls.append(&scan_spinner);
        scan_controls.append(&scan_btn);
        add_group.set_header_suffix(Some(&scan_controls));
        prefs_page.add(&add_group);

        // ── Settings ─────────────────────────────────────────────────────────
        let auto_reconnect = Rc::new(Cell::new(true));
        let settings_group = adw::PreferencesGroup::builder().title("Settings").build();
        let auto_switch = adw::SwitchRow::builder()
            .title("Reconnect automatically")
            .subtitle("Connect to saved devices as soon as a scan finds them")
            .active(true)
            .build();
        let auto_reconnect_switch = Rc::clone(&auto_reconnect);
        auto_switch.connect_active_notify(move |row| {
            auto_reconnect_switch.set(row.is_active());
        });
        settings_group.add(&auto_switch);
        prefs_page.add(&settings_group);

        prefs_page.set_vexpand(true);
        root.append(&prefs_page);

        let ctx = PageCtx {
            cmd_tx,
            pool,
            rt: rt_handle,
            entries: Rc::new(RefCell::new(HashMap::new())),
            slots: Rc::new(slots),
            my_devices_group,
            add_group,
            scan_btn: scan_btn.clone(),
            scan_spinner,
            scanning: Rc::new(Cell::new(false)),
            scan_generation: Rc::new(Cell::new(0)),
            auto_reconnect,
        };

        let ctx_scan = ctx.clone();
        scan_btn.connect_clicked(move |_| begin_scan(&ctx_scan));

        // Populate saved devices.
        for saved in &saved_devices {
            let transport = Transport::from_db_str(&saved.transport);
            let mut kind = DeviceType::from_db_str(&saved.device_type);
            // Rows saved before the device_type column existed: an ANT+ device
            // can only be an FE-C trainer, so classify it without a reconnect.
            if kind == DeviceType::Unknown && transport == Transport::AntPlus {
                kind = DeviceType::FtmsTrainer;
            }
            let (row, status_label) = make_saved_row(
                &ctx,
                &saved.address,
                &saved.display_name,
                kind,
                transport,
                RowStatus::Idle,
            );
            ctx.my_devices_group.add(&row);
            ctx.entries.borrow_mut().insert(
                saved.address.clone(),
                DeviceEntry {
                    display_name: saved.display_name.clone(),
                    rssi: None,
                    transport,
                    kind,
                    saved: true,
                    connected: false,
                    connecting: false,
                    seen: false,
                    erg_enabled: saved.erg_enabled,
                    row,
                    status_label: Some(status_label),
                },
            );
        }

        refresh_strip(&ctx);

        // Auto-start a scan on launch when saved devices exist so they reconnect
        // without the user having to navigate to the Devices page first.
        if !saved_devices.is_empty() {
            begin_scan(&ctx);
        }

        Self { root, ctx }
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
        kind: DeviceType,
    ) {
        let mut auto_connect = false;
        {
            let mut entries = self.ctx.entries.borrow_mut();
            if let Some(entry) = entries.get_mut(&address) {
                entry.rssi = rssi;
                entry.transport = transport;
                entry.seen = true;
                if entry.kind == DeviceType::Unknown && kind != DeviceType::Unknown {
                    entry.kind = kind;
                }
                if entry.saved {
                    if !entry.connected && !entry.connecting && self.ctx.auto_reconnect.get() {
                        entry.connecting = true;
                        auto_connect = true;
                    }
                } else {
                    entry
                        .row
                        .set_subtitle(&add_row_subtitle(entry.kind, transport, rssi));
                }
            } else {
                let row = make_add_row(&self.ctx, &address, &name, kind, transport, rssi);
                self.ctx.add_group.add(&row);
                entries.insert(
                    address.clone(),
                    DeviceEntry {
                        display_name: name,
                        rssi,
                        transport,
                        kind,
                        saved: false,
                        connected: false,
                        connecting: false,
                        seen: true,
                        erg_enabled: true,
                        row,
                        status_label: None,
                    },
                );
            }
        }

        if auto_connect {
            set_saved_status(&self.ctx, &address, RowStatus::Connecting);
            let _ = self.ctx.cmd_tx.try_send(DeviceCommand::Connect { address });
        }
    }

    pub fn on_connection_changed(
        &mut self,
        address: String,
        connected: bool,
        device_type: Option<DeviceType>,
    ) {
        let newly_saved = {
            let mut entries = self.ctx.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&address) else {
                // Already forgotten (e.g. disconnect event after Forget) — nothing to update.
                return;
            };
            entry.connecting = false;
            entry.connected = connected;
            if let Some(t) = device_type {
                if t != DeviceType::Unknown {
                    entry.kind = t;
                }
            }
            let newly_saved = connected && !entry.saved;
            if newly_saved {
                entry.saved = true;
                self.ctx.add_group.remove(&entry.row);
            }
            newly_saved
        };

        // Rebuild the saved row so icon, subtitle, and status reflect the new state.
        rebuild_saved_row(&self.ctx, &address);
        if newly_saved {
            self.ctx.my_devices_group.set_visible(true);
        }
        refresh_strip(&self.ctx);

        if connected {
            let (display_name, transport, kind, erg_enabled) = {
                let entries = self.ctx.entries.borrow();
                let e = &entries[&address];
                (e.display_name.clone(), e.transport, e.kind, e.erg_enabled)
            };

            let pool = self.ctx.pool.clone();
            let addr = address.clone();
            self.ctx.rt.spawn(async move {
                if let Err(e) = db::save_device(
                    &pool,
                    &addr,
                    &display_name,
                    transport.as_db_str(),
                    kind.as_db_str(),
                )
                .await
                {
                    tracing::error!("save_device failed: {e}");
                }
            });

            // Sync the persisted ERG preference to the manager for trainers.
            if kind == DeviceType::FtmsTrainer {
                let _ = self
                    .ctx
                    .cmd_tx
                    .try_send(DeviceCommand::SetErgMode(erg_enabled));
            }

            // Keep searching while other saved devices are still missing (the
            // manager stops the BLE scan to connect, so it must be restarted);
            // once everything saved is connected, stop.
            if self.ctx.scanning.get() {
                if has_missing_saved(&self.ctx) {
                    let _ = self.ctx.cmd_tx.try_send(DeviceCommand::StartScan);
                } else {
                    let _ = self.ctx.cmd_tx.try_send(DeviceCommand::StopScan);
                    finish_scan(&self.ctx);
                }
            }
        }
    }

    /// Return the human-readable display name for a device address, falling back to the address.
    pub fn display_name_for(&self, address: &str) -> String {
        self.ctx
            .entries
            .borrow()
            .get(address)
            .map(|e| e.display_name.clone())
            .unwrap_or_else(|| address.to_string())
    }
}

// ── Setup strip ──────────────────────────────────────────────────────────────

fn make_setup_strip() -> (gtk::Box, SlotSet) {
    let strip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build();

    let trainer = make_slot_card(&strip, DeviceType::FtmsTrainer.icon_name(), "Trainer");
    let heart_rate = make_slot_card(
        &strip,
        DeviceType::HeartRateMonitor.icon_name(),
        "Heart rate",
    );
    let cadence = make_slot_card(&strip, DeviceType::CadenceSensor.icon_name(), "Cadence");

    (
        strip,
        SlotSet {
            trainer,
            heart_rate,
            cadence,
        },
    )
}

fn make_slot_card(strip: &gtk::Box, icon_name: &str, role: &str) -> SetupSlot {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["card"])
        .build();

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .css_classes(["dim-label"])
        .build();
    let role_label = gtk::Label::builder()
        .label(role)
        .css_classes(["caption-heading", "dim-label"])
        .xalign(0.0)
        .build();
    header.append(&icon);
    header.append(&role_label);
    card.append(&header);

    let name_label = gtk::Label::builder()
        .label("—")
        .css_classes(["heading"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(12)
        .margin_end(12)
        .build();
    card.append(&name_label);

    let status_label = gtk::Label::builder()
        .label("—")
        .css_classes(["caption", "dim-label"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    card.append(&status_label);

    strip.append(&card);
    SetupSlot {
        icon,
        name_label,
        status_label,
    }
}

/// Recompute all three role cards from the current entries.
fn refresh_strip(ctx: &PageCtx) {
    let entries = ctx.entries.borrow();
    let scanning = ctx.scanning.get();

    let slot_views: [(&SetupSlot, &[DeviceType], &str); 3] = [
        (
            &ctx.slots.trainer,
            &[DeviceType::FtmsTrainer, DeviceType::CyclingPowerMeter],
            "No trainer yet",
        ),
        (
            &ctx.slots.heart_rate,
            &[DeviceType::HeartRateMonitor],
            "No monitor yet",
        ),
        (
            &ctx.slots.cadence,
            &[DeviceType::CadenceSensor],
            "No sensor yet",
        ),
    ];

    for (slot, kinds, empty_name) in slot_views {
        let connected = entries
            .values()
            .find(|e| e.saved && e.connected && kinds.contains(&e.kind));
        let saved = entries
            .values()
            .find(|e| e.saved && kinds.contains(&e.kind));

        match (connected, saved) {
            (Some(e), _) => set_slot(slot, &e.display_name, "Connected", true),
            (None, Some(e)) => {
                let status = if scanning {
                    "Searching…"
                } else {
                    "Not connected"
                };
                set_slot(slot, &e.display_name, status, false);
            }
            (None, None) => set_slot(slot, empty_name, "Scan to add one", false),
        }
    }
}

fn set_slot(slot: &SetupSlot, name: &str, status: &str, connected: bool) {
    slot.name_label.set_label(name);
    slot.status_label.set_label(status);
    if connected {
        slot.icon.set_css_classes(&["success"]);
        slot.status_label.set_css_classes(&["caption", "success"]);
    } else {
        slot.icon.set_css_classes(&["dim-label"]);
        slot.status_label.set_css_classes(&["caption", "dim-label"]);
    }
}

// ── Scanning ─────────────────────────────────────────────────────────────────

fn begin_scan(ctx: &PageCtx) {
    if ctx.scanning.get() {
        return;
    }
    ctx.scanning.set(true);
    let generation = ctx.scan_generation.get().wrapping_add(1);
    ctx.scan_generation.set(generation);

    let _ = ctx.cmd_tx.try_send(DeviceCommand::StartScan);
    ctx.scan_spinner.set_visible(true);
    ctx.scan_spinner.start();
    ctx.scan_btn.set_sensitive(false);
    ctx.scan_btn.set_label("Scanning…");
    ctx.add_group.set_description(Some(ADD_DESC_SCANNING));
    update_saved_statuses(ctx);
    refresh_strip(ctx);

    // Give up after a while so a fruitless scan ends with guidance, not silence.
    let ctx_timeout = ctx.clone();
    glib::timeout_add_local_once(SCAN_TIMEOUT, move || {
        if !ctx_timeout.scanning.get() || ctx_timeout.scan_generation.get() != generation {
            return; // scan already ended, or a newer scan is running
        }
        let _ = ctx_timeout.cmd_tx.try_send(DeviceCommand::StopScan);
        finish_scan(&ctx_timeout);
        let found_any_new = ctx_timeout.entries.borrow().values().any(|e| !e.saved);
        if !found_any_new {
            ctx_timeout
                .add_group
                .set_description(Some(ADD_DESC_NOTHING_FOUND));
        }
    });
}

fn finish_scan(ctx: &PageCtx) {
    ctx.scanning.set(false);
    ctx.scan_spinner.stop();
    ctx.scan_spinner.set_visible(false);
    ctx.scan_btn.set_sensitive(true);
    ctx.scan_btn.set_label("Scan");
    ctx.add_group.set_description(Some(ADD_DESC_IDLE));
    update_saved_statuses(ctx);
    refresh_strip(ctx);
}

fn has_missing_saved(ctx: &PageCtx) -> bool {
    ctx.entries
        .borrow()
        .values()
        .any(|e| e.saved && !e.connected)
}

/// Refresh status text on every saved-but-not-connected row.
fn update_saved_statuses(ctx: &PageCtx) {
    let scanning = ctx.scanning.get();
    let entries = ctx.entries.borrow();
    for entry in entries.values() {
        if !entry.saved || entry.connected {
            continue;
        }
        let status = if entry.connecting {
            RowStatus::Connecting
        } else if scanning {
            RowStatus::Searching
        } else {
            RowStatus::Idle
        };
        if let Some(label) = &entry.status_label {
            apply_status(label, status);
        }
    }
}

// ── Row construction ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum RowStatus {
    Connected,
    Connecting,
    Searching,
    Idle,
}

fn apply_status(label: &gtk::Label, status: RowStatus) {
    let (text, classes): (&str, &[&str]) = match status {
        RowStatus::Connected => ("Connected", &["caption", "success"]),
        RowStatus::Connecting => ("Connecting…", &["caption", "dim-label"]),
        RowStatus::Searching => ("Searching…", &["caption", "dim-label"]),
        RowStatus::Idle => ("Not connected", &["caption", "dim-label"]),
    };
    label.set_label(text);
    label.set_css_classes(classes);
}

fn make_saved_row(
    ctx: &PageCtx,
    address: &str,
    display_name: &str,
    kind: DeviceType,
    transport: Transport,
    status: RowStatus,
) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder()
        .title(display_name)
        .subtitle(format!("{} · {}", kind.label(), transport.label()))
        .activatable(true)
        .tooltip_text("Open device details")
        .build();

    row.add_prefix(
        &gtk::Image::builder()
            .icon_name(kind.icon_name())
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );

    let status_label = gtk::Label::builder().valign(gtk::Align::Center).build();
    apply_status(&status_label, status);
    row.add_suffix(&status_label);
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("go-next-symbolic")
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );

    let ctx_dialog = ctx.clone();
    let addr = address.to_string();
    row.connect_activated(move |row| {
        open_device_dialog(&ctx_dialog, &addr, row);
    });

    (row, status_label)
}

/// Tear down and re-add a saved device's row so it reflects the entry's state.
fn rebuild_saved_row(ctx: &PageCtx, address: &str) {
    let Some((old_row, display_name, kind, transport, status)) = ({
        let entries = ctx.entries.borrow();
        entries.get(address).map(|e| {
            let status = if e.connected {
                RowStatus::Connected
            } else if e.connecting {
                RowStatus::Connecting
            } else if ctx.scanning.get() {
                RowStatus::Searching
            } else {
                RowStatus::Idle
            };
            (
                e.row.clone(),
                e.display_name.clone(),
                e.kind,
                e.transport,
                status,
            )
        })
    }) else {
        return;
    };

    let (new_row, status_label) =
        make_saved_row(ctx, address, &display_name, kind, transport, status);
    ctx.my_devices_group.remove(&old_row);
    ctx.my_devices_group.add(&new_row);

    if let Some(entry) = ctx.entries.borrow_mut().get_mut(address) {
        entry.row = new_row;
        entry.status_label = Some(status_label);
    }
}

fn add_row_subtitle(kind: DeviceType, transport: Transport, rssi: Option<i16>) -> String {
    match signal_text(rssi) {
        Some(signal) => format!("{} · {} · {}", kind.label(), transport.label(), signal),
        None => format!("{} · {}", kind.label(), transport.label()),
    }
}

fn make_add_row(
    ctx: &PageCtx,
    address: &str,
    name: &str,
    kind: DeviceType,
    transport: Transport,
    rssi: Option<i16>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(name)
        .subtitle(add_row_subtitle(kind, transport, rssi))
        .build();

    row.add_prefix(
        &gtk::Image::builder()
            .icon_name(kind.icon_name())
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );

    let add_btn = gtk::Button::builder()
        .label("Add")
        .css_classes(["pill", "suggested-action"])
        .valign(gtk::Align::Center)
        .tooltip_text("Connect and save this device")
        .build();

    let addr = address.to_string();
    let ctx_add = ctx.clone();
    add_btn.connect_clicked(move |btn| {
        btn.set_sensitive(false);
        btn.set_label("Connecting…");
        if let Some(e) = ctx_add.entries.borrow_mut().get_mut(&addr) {
            e.connecting = true;
        }
        let _ = ctx_add.cmd_tx.try_send(DeviceCommand::Connect {
            address: addr.clone(),
        });
    });

    row.add_suffix(&add_btn);
    row
}

// ── Device detail dialog ─────────────────────────────────────────────────────

fn open_device_dialog(ctx: &PageCtx, address: &str, parent: &impl IsA<gtk::Widget>) {
    // Snapshot the entry — the dialog shows state as of opening.
    let Some((display_name, kind, transport, rssi, connected, seen, erg_enabled)) = ({
        let entries = ctx.entries.borrow();
        entries.get(address).map(|e| {
            (
                e.display_name.clone(),
                e.kind,
                e.transport,
                e.rssi,
                e.connected,
                e.seen,
                e.erg_enabled,
            )
        })
    }) else {
        return;
    };

    let dialog = adw::Dialog::builder()
        .title(&display_name)
        .content_width(420)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    // ── Name + trainer options ───────────────────────────────────────────────
    let options_group = adw::PreferencesGroup::new();

    let name_row = adw::EntryRow::builder()
        .title("Name")
        .text(&display_name)
        .show_apply_button(true)
        .build();
    let ctx_rename = ctx.clone();
    let addr_rename = address.to_string();
    name_row.connect_apply(move |row| {
        let new_name = row.text().to_string();
        if new_name.is_empty() {
            return;
        }
        if let Some(e) = ctx_rename.entries.borrow_mut().get_mut(&addr_rename) {
            e.display_name = new_name.clone();
            e.row.set_title(&new_name);
        }
        refresh_strip(&ctx_rename);
        let pool = ctx_rename.pool.clone();
        let addr = addr_rename.clone();
        ctx_rename.rt.spawn(async move {
            if let Err(e) = db::rename_device(&pool, &addr, &new_name).await {
                tracing::error!("rename_device failed: {e}");
            }
        });
    });
    options_group.add(&name_row);

    if kind == DeviceType::FtmsTrainer {
        let erg_row = adw::SwitchRow::builder()
            .title("Automatic resistance (ERG)")
            .subtitle("The trainer adjusts itself to match each interval's target power")
            .active(erg_enabled)
            .build();
        let ctx_erg = ctx.clone();
        let addr_erg = address.to_string();
        erg_row.connect_active_notify(move |sw| {
            let enabled = sw.is_active();
            if let Some(e) = ctx_erg.entries.borrow_mut().get_mut(&addr_erg) {
                e.erg_enabled = enabled;
            }
            let _ = ctx_erg.cmd_tx.try_send(DeviceCommand::SetErgMode(enabled));
            let pool = ctx_erg.pool.clone();
            let addr = addr_erg.clone();
            ctx_erg.rt.spawn(async move {
                if let Err(e) = db::set_device_erg_enabled(&pool, &addr, enabled).await {
                    tracing::error!("set_device_erg_enabled failed: {e}");
                }
            });
        });
        options_group.add(&erg_row);
    }
    content.append(&options_group);

    // ── Details ──────────────────────────────────────────────────────────────
    let details_group = adw::PreferencesGroup::builder().title("Details").build();
    details_group.add(&property_row("Type", kind.label()));
    details_group.add(&property_row("Connection", transport.label()));
    if let Some(signal) = signal_text(rssi) {
        details_group.add(&property_row("Signal", signal));
    }
    details_group.add(&property_row("Address", address));
    content.append(&details_group);

    // ── Actions ──────────────────────────────────────────────────────────────
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .build();

    if connected {
        let disconnect_btn = gtk::Button::builder()
            .label("Disconnect")
            .css_classes(["pill"])
            .tooltip_text("Disconnect this device but keep it saved")
            .build();
        let ctx_disc = ctx.clone();
        let addr_disc = address.to_string();
        let dialog_disc = dialog.clone();
        disconnect_btn.connect_clicked(move |_| {
            let _ = ctx_disc.cmd_tx.try_send(DeviceCommand::Disconnect {
                address: addr_disc.clone(),
            });
            dialog_disc.close();
        });
        actions.append(&disconnect_btn);
    } else if seen {
        let connect_btn = gtk::Button::builder()
            .label("Connect")
            .css_classes(["pill", "suggested-action"])
            .tooltip_text("Connect to this device")
            .build();
        let ctx_conn = ctx.clone();
        let addr_conn = address.to_string();
        let dialog_conn = dialog.clone();
        connect_btn.connect_clicked(move |_| {
            if let Some(e) = ctx_conn.entries.borrow_mut().get_mut(&addr_conn) {
                e.connecting = true;
            }
            set_saved_status(&ctx_conn, &addr_conn, RowStatus::Connecting);
            let _ = ctx_conn.cmd_tx.try_send(DeviceCommand::Connect {
                address: addr_conn.clone(),
            });
            dialog_conn.close();
        });
        actions.append(&connect_btn);
    } else {
        // Never discovered this session — the manager can't connect to it yet.
        let hint = gtk::Label::builder()
            .label("Turn the device on and scan to connect")
            .css_classes(["caption", "dim-label"])
            .build();
        actions.append(&hint);
    }

    let forget_btn = gtk::Button::builder()
        .label("Forget This Device…")
        .css_classes(["pill", "destructive-action"])
        .tooltip_text("Remove this device from your saved devices")
        .build();
    let ctx_forget = ctx.clone();
    let addr_forget = address.to_string();
    let dialog_forget = dialog.clone();
    forget_btn.connect_clicked(move |_| {
        confirm_forget(&ctx_forget, &addr_forget, &dialog_forget);
    });
    actions.append(&forget_btn);
    content.append(&actions);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

/// Read-only "Title: value" row for the detail dialog.
fn property_row(title: &str, value: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .css_classes(["property"])
        .subtitle_selectable(true)
        .build()
}

fn confirm_forget(ctx: &PageCtx, address: &str, device_dialog: &adw::Dialog) {
    let confirm = adw::AlertDialog::builder()
        .heading("Forget Device?")
        .body("This device will be removed from your saved devices. You can add it again with a scan.")
        .build();
    confirm.add_response("cancel", "_Cancel");
    confirm.add_response("forget", "_Forget");
    confirm.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
    confirm.set_close_response("cancel");

    let ctx = ctx.clone();
    let address = address.to_string();
    let dialog_to_close = device_dialog.clone();
    confirm.connect_response(None, move |_, resp| {
        if resp != "forget" {
            return;
        }
        let removed = ctx.entries.borrow_mut().remove(&address);
        if let Some(entry) = removed {
            if entry.saved {
                ctx.my_devices_group.remove(&entry.row);
            } else {
                ctx.add_group.remove(&entry.row);
            }
            if entry.connected {
                // on_connection_changed will be a no-op: the entry is gone.
                let _ = ctx.cmd_tx.try_send(DeviceCommand::Disconnect {
                    address: address.clone(),
                });
            }
        }
        let any_saved = ctx.entries.borrow().values().any(|e| e.saved);
        ctx.my_devices_group.set_visible(any_saved);
        refresh_strip(&ctx);

        let pool = ctx.pool.clone();
        let addr = address.clone();
        ctx.rt.spawn(async move {
            if let Err(e) = db::delete_device(&pool, &addr).await {
                tracing::error!("delete_device failed: {e}");
            }
        });
        dialog_to_close.close();
    });

    confirm.present(Some(device_dialog));
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn set_saved_status(ctx: &PageCtx, address: &str, status: RowStatus) {
    let entries = ctx.entries.borrow();
    if let Some(label) = entries.get(address).and_then(|e| e.status_label.as_ref()) {
        apply_status(label, status);
    }
}

/// Signal strength in words — thresholds match `PeripheralInfo::signal_bars`.
fn signal_text(rssi: Option<i16>) -> Option<&'static str> {
    Some(match rssi? {
        r if r >= -55 => "Excellent signal",
        r if r >= -67 => "Good signal",
        r if r >= -80 => "Fair signal",
        _ => "Weak signal",
    })
}
