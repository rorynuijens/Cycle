pub mod markdown;
pub mod pages;
pub mod preferences;
pub mod resources;
pub mod widgets;
pub mod window;

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
