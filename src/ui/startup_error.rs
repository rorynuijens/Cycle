//! The window shown when Cycle cannot start at all.
//!
//! This used to be an `adw::AlertDialog` presented with no parent. With no
//! window of its own the application holds nothing open, so GApplication quit
//! the moment `activate` returned and the one message meant to explain a failed
//! start flashed on screen for a quarter of a second. A real window keeps the
//! application alive until the rider closes it, and gives the explanation room
//! to be read — and selected, so it can be pasted into a bug report.

use adw::prelude::*;
use gtk::glib;

/// Build the window that says why Cycle could not start. Closing it, or
/// pressing Quit, ends the application: it has no other window to fall back on.
pub fn build(app: &adw::Application, heading: &str, message: &str) -> adw::ApplicationWindow {
    let body = gtk::Label::builder()
        .label(message)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .justify(gtk::Justification::Center)
        .build();

    let quit_btn = gtk::Button::builder()
        .label("_Quit")
        .use_underline(true)
        .halign(gtk::Align::Center)
        .tooltip_text("Close Cycle")
        .build();
    quit_btn.add_css_class("pill");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .build();
    content.append(&body);
    content.append(&quit_btn);

    let status = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title(heading)
        .child(
            &adw::Clamp::builder()
                .maximum_size(480)
                .child(&content)
                .build(),
        )
        .build();

    // Compact: the full-size icon pushed Quit below the fold of a window
    // that has nothing else to show.
    status.add_css_class("compact");

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&status));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Cycle")
        .default_width(560)
        .default_height(520)
        .content(&view)
        .build();

    // Focus on the button, not the text: a selectable label that takes focus
    // opens with every word of it highlighted. Enter then closes the window.
    window.set_default_widget(Some(&quit_btn));
    GtkWindowExt::set_focus(&window, Some(&quit_btn));

    quit_btn.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));

    window
}
