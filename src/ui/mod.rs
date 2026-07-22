pub mod markdown;
pub mod pages;
pub mod preferences;
pub mod resources;
pub mod widgets;
pub mod window;

/// App-wide stylesheet defining the `display` typography class from the
/// CLAUDE.md §1.5 type scale (hero numbers, e.g. live power), which libadwaita
/// itself does not ship. Sizes are relative so the class follows the user's
/// system font size; no colours are defined, so both themes work unchanged.
const APP_CSS: &str = "
.display {
    font-size: 400%;
    font-weight: 800;
}
";

/// Install the app stylesheet on the default display. Call once at activate,
/// before the main window is built.
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(APP_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Run an async task on the tokio runtime and deliver its result to `on_done`
/// back on the GTK main thread — without ever blocking the GLib loop.
///
/// This is the non-blocking replacement for `rt.block_on(...)` in GTK callbacks
/// (see CLAUDE.md §2.3): the future runs on the tokio runtime, its result is sent
/// over an `async_channel`, and `on_done` is invoked from the GLib main context,
/// so it is safe to touch widgets inside it.
///
/// `on_done` runs at most once. If the task is dropped before completing (e.g. the
/// runtime shuts down) it simply never fires.
pub fn spawn_to_main<T, Fut>(
    rt: &tokio::runtime::Handle,
    fut: Fut,
    on_done: impl FnOnce(T) + 'static,
) where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    rt.spawn(async move {
        let _ = tx.send(fut.await).await;
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(value) = rx.recv().await {
            on_done(value);
        }
    });
}
