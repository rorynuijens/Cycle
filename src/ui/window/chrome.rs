//! The window's frame: the sidebar, the header bars, and fullscreen.

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::training::engine::{EngineState, WorkoutEngine};
use crate::ui::pages::player::PlayerPage;

/// Build the sidebar's navigation list and wire it to the page stack.
pub fn build_sidebar_list(
    stack: &adw::ViewStack,
    nav_items: &[(&str, &str, &str)],
) -> gtk::ListBox {
    let sidebar_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();

    for (label, icon, page_name) in nav_items {
        sidebar_list.append(&make_nav_row(label, icon, page_name));
    }

    if let Some(first_row) = sidebar_list.row_at_index(0) {
        sidebar_list.select_row(Some(&first_row));
    }

    let stack = stack.clone();
    sidebar_list.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            let page_name = row.widget_name();
            let page_name = page_name.as_str();
            if stack.child_by_name(page_name).is_some() {
                stack.set_visible_child_name(page_name);
            }
        }
    });

    sidebar_list
}

/// Wrap the navigation list in its header bar and navigation page.
pub fn build_sidebar_page(sidebar_list: &gtk::ListBox) -> adw::NavigationPage {
    let sidebar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    sidebar_box.append(
        &adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .build(),
    );
    sidebar_box.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(sidebar_list)
            .build(),
    );

    adw::NavigationPage::builder()
        .title("Cycle")
        .tag("sidebar")
        .child(&sidebar_box)
        .build()
}

/// The content side: header bar, the page stack, and the split view holding both.
///
/// Returns the fullscreen-exit button, which the window shows and hides as the
/// fullscreen state changes.
pub fn build_content(
    content_nav_page: &adw::NavigationPage,
    sidebar_nav_page: &adw::NavigationPage,
    stack: &adw::ViewStack,
    back_btn: &gtk::Button,
    start_btn: &gtk::Button,
    engine_rc: Rc<RefCell<WorkoutEngine>>,
    player_rc: Rc<RefCell<PlayerPage>>,
) -> (adw::NavigationSplitView, gtk::Button) {
    let content_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let content_header = adw::HeaderBar::new();

    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .build();
    let main_menu = gio::Menu::new();
    main_menu.append(Some("Preferences"), Some("app.preferences"));
    main_menu.append(Some("About Cycle"), Some("app.about"));
    menu_button.set_menu_model(Some(&main_menu));
    content_header.pack_end(&menu_button);

    let fs_exit_btn = gtk::Button::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Exit Fullscreen")
        .css_classes(["flat", "circular"])
        .visible(false)
        .build();
    content_header.pack_end(&fs_exit_btn);

    content_header.pack_start(back_btn);
    content_header.pack_start(start_btn);

    // Back button: navigate to dashboard from summary page
    let stack_for_back = stack.clone();
    back_btn.connect_clicked(move |_| {
        stack_for_back.set_visible_child_name("dashboard");
    });

    // Return to the active player. If the workout is paused, this also
    // resumes it — the button reads "Resume Workout" and users expect that.
    let stack_for_btn = stack.clone();
    let engine_for_btn = Rc::clone(&engine_rc);
    let player_for_btn = Rc::clone(&player_rc);
    start_btn.connect_clicked(move |_| {
        if engine_for_btn.borrow().state == EngineState::Paused {
            player_for_btn.borrow().trigger_pause_toggle();
        }
        stack_for_btn.set_visible_child_name("player");
    });

    content_box.append(&content_header);
    content_box.append(stack);

    // Set the child now that content_box is fully assembled
    content_nav_page.set_child(Some(&content_box));

    let split_view = adw::NavigationSplitView::builder()
        .sidebar(sidebar_nav_page)
        .content(content_nav_page)
        .sidebar_width_fraction(0.22)
        .min_sidebar_width(200.0)
        .max_sidebar_width(280.0)
        .build();

    (split_view, fs_exit_btn)
}

/// F11 toggles fullscreen; the header's exit button leaves it.
pub fn connect_fullscreen(window: &adw::ApplicationWindow, fs_exit_btn: &gtk::Button) {
    let fs_btn_notify = fs_exit_btn.clone();
    window.connect_fullscreened_notify(move |win| {
        fs_btn_notify.set_visible(win.is_fullscreen());
    });

    let window_unfull = window.clone();
    fs_exit_btn.connect_clicked(move |_| {
        window_unfull.unfullscreen();
    });
    let window_for_key = window.clone();
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::F11 {
            if window_for_key.is_fullscreen() {
                window_for_key.unfullscreen();
            } else {
                window_for_key.fullscreen();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

pub fn make_nav_row(label: &str, icon_name: &str, page_name: &str) -> gtk::ListBoxRow {
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .icon_size(gtk::IconSize::Normal)
        .build();

    let text = gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();

    row_box.append(&icon);
    row_box.append(&text);

    gtk::ListBoxRow::builder()
        .child(&row_box)
        .name(page_name)
        .build()
}
