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
        // Make the bundled symbolic icons (gresource icons/scalable/actions/…)
        // resolvable by name. GtkApplication normally registers the base path
        // itself; adding it explicitly keeps icon lookup independent of how
        // the application id maps to a resource base path.
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_resource_path("/io/github/rorynuijens/Cycle/icons");
    }
}

/// Why an AI request produced no answer.
///
/// The two cases need different wording: one points at the rider's API key, the
/// other at their database. Telling someone to check their key when the database
/// is at fault sends them to the wrong place, and the wording is worth keeping
/// identical across the pages that ask the coach for something.
#[derive(Debug, Clone, Copy)]
pub enum AiFailure {
    /// The training history could not be read, so nothing was sent.
    DataUnavailable,
    /// The request was sent but did not come back with an answer.
    Request,
}

impl AiFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::DataUnavailable => {
                "Could not read your training history, so nothing was sent to the AI Coach."
            }
            Self::Request => {
                "The AI Coach couldn't complete this request. \
                 Please check your API key and try again."
            }
        }
    }
}

/// Run an async task on the tokio runtime and deliver its result to `on_done`
/// back on the GTK main thread — without ever blocking the GLib loop.
///
/// This is the non-blocking replacement for `rt.block_on(...)` in GTK callbacks
/// (see CLAUDE.md §2.3): the future runs on the tokio runtime, its result is sent
/// over an `async_channel`, and `on_done` is invoked from the GLib main context,
/// A page's reload callback.
pub type ReloadFn = std::rc::Rc<dyn Fn()>;

/// A reload callback a page holds a reference to *inside itself*.
///
/// Every page hits the same problem: a row's delete or edit button has to
/// trigger a rebuild of the list it lives in, but that rebuild closure does not
/// exist yet when the row is built. The holder is filled in once the closure is
/// made, and the callbacks read it when they fire.
pub type ReloadHolder = std::rc::Rc<std::cell::RefCell<Option<ReloadFn>>>;

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
