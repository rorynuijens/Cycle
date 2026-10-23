mod ai;
mod data;
mod devices;
mod training;
mod ui;

use adw::prelude::*;
use gtk::glib;
use tracing::info;
use ui::window::CycleGtkWindow;

const APP_ID: &str = "io.github.rorynuijens.Cycle";

fn main() -> glib::ExitCode {
    // Register bundled GLib resources (RPE thumbnails) before any widget is created.
    gio::resources_register_include!("cycle.gresource").expect("failed to register GLib resources");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cycle=debug".parse().unwrap()),
        )
        .init();

    info!("Starting Cycle");

    // Tokio runtime lives on background thread(s); GLib owns the main thread.
    let rt = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );

    // BLE device manager — runs for the lifetime of the app.
    let (device_manager, cmd_tx, event_rx) = devices::manager::DeviceManager::new();
    rt.spawn(async move {
        if let Err(e) = device_manager.run().await {
            tracing::error!("DeviceManager error: {e}");
        }
    });

    // Open DB, seed default workouts, load athlete profile.
    let pool = match rt.block_on(data::db::open()) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            // Explain before giving up, so the rider sees a human-readable message
            // rather than a raw panic backtrace — in a window of its own, which
            // is what keeps the application alive long enough to read it.
            let app = adw::Application::builder()
                .application_id(APP_ID)
                .flags(gio::ApplicationFlags::empty())
                .build();
            // `{e:#}` prints anyhow's whole context chain: a refused schema version
            // or a failed migration step explains itself in the outer message, and
            // the cause underneath it is what makes the difference diagnosable.
            // The path is resolved rather than written out, because it differs
            // between a flatpak install and a development build.
            let msg = format!(
                "Tour du Salon could not open its database.\n\n{e:#}\n\nDatabase location:\n{}",
                data::paths::data_dir().display()
            );
            app.connect_activate(move |app| {
                ui::startup_error::build(app, "Tour du Salon Could Not Start", &msg).present();
            });
            return app.run();
        }
    };

    // Migrate any API keys stored in plaintext DB settings to the system keyring.
    if let Err(e) = rt.block_on(data::db::migrate_secrets_to_keyring(&pool)) {
        tracing::warn!("Secret migration failed (non-fatal): {e}");
    }

    rt.block_on(data::db::seed_workouts(&pool))
        .expect("failed to seed workouts");

    let athlete = rt
        .block_on(data::db::load_or_create_athlete(&pool))
        .expect("failed to load athlete");

    let workouts = rt
        .block_on(data::db::load_workouts(&pool))
        .expect("failed to load workouts");

    // Default to the first workout; fall back to the in-memory sample if DB is empty.
    let workout = workouts
        .first()
        .cloned()
        .unwrap_or_else(data::workout::Workout::sample_threshold);

    let rt_handle = rt.handle().clone();
    let saved_devices = rt
        .block_on(data::db::load_saved_devices(&pool))
        .unwrap_or_default();
    let device_radio = rt
        .block_on(data::settings::device_radio(&pool))
        .unwrap_or_else(|e| {
            tracing::warn!("Could not read the device radio, using ANT+: {e}");
            devices::peripheral::Transport::AntPlus
        });
    // Sent before the window exists, so the manager is on the right radio
    // before the Devices page asks it to scan.
    let _ = cmd_tx.try_send(devices::manager::DeviceCommand::SetRadio(device_radio));

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::empty())
        .build();

    app.connect_activate(move |app| {
        // Register the local icon directory so the app icon is found by name during development.
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_search_path("data/icons");
        }
        // App stylesheet (defines the `display` typography class — CLAUDE.md §1.5).
        ui::load_css();

        let window = CycleGtkWindow::new(
            app,
            cmd_tx.clone(),
            event_rx.clone(),
            pool.clone(),
            rt_handle.clone(),
            saved_devices.clone(),
            device_radio,
            athlete.clone(),
            workout.clone(),
            workouts.clone(),
        );
        window.present();
    });

    // GLib loop blocks here; `rt` stays alive until app.run() returns.
    app.run()
}
