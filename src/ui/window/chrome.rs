//! The window's frame: the sidebar, the header bars, and fullscreen.

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
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

/// The window's frame, and the parts of it fullscreen has to reach.
///
/// `root` is what goes into the toast overlay; everything else is handed to
/// [`connect_fullscreen`], which hides and collapses them.
pub struct ContentChrome {
    /// The widget to install as the window's content.
    pub root: gtk::Overlay,
    pub split_view: adw::NavigationSplitView,
    /// The page stack, so leaving fullscreen can put back the page fullscreen
    /// was showing — see [`connect_fullscreen`].
    pub stack: adw::ViewStack,
    /// Hidden entirely in fullscreen — see [`connect_fullscreen`].
    pub header: adw::HeaderBar,
    /// Leaves fullscreen from the header bar, so it is only useful windowed…
    pub fs_exit_btn: gtk::Button,
    /// …and this one floats over the page, for when the header is gone.
    pub fs_float_btn: gtk::Button,
}

/// The content side: header bar, the page stack, and the split view holding both.
// Assembling the window's frame means naming every part of it; splitting the
// list up would only move the arguments somewhere else.
#[allow(clippy::too_many_arguments)]
pub fn build_content(
    content_nav_page: &adw::NavigationPage,
    sidebar_nav_page: &adw::NavigationPage,
    stack: &adw::ViewStack,
    back_btn: &gtk::Button,
    start_btn: &gtk::Button,
    route_timer_alive: Rc<Cell<bool>>,
    engine_rc: Rc<RefCell<WorkoutEngine>>,
    player_rc: Rc<RefCell<PlayerPage>>,
) -> ContentChrome {
    let content_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let content_header = adw::HeaderBar::new();

    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .build();
    let main_menu = gio::Menu::new();
    main_menu.append(Some("Fullscreen"), Some("win.toggle-fullscreen"));
    main_menu.append(Some("Preferences"), Some("app.preferences"));
    main_menu.append(Some("About Cycle"), Some("app.about"));
    menu_button.set_menu_model(Some(&main_menu));
    content_header.pack_end(&menu_button);

    // Both fullscreen buttons drive the action rather than capturing the window
    // in a closure. A handler on a button the window owns would capture the
    // window right back, and GTK has no cycle collector (CLAUDE.md §2.4).
    let fs_exit_btn = gtk::Button::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Exit Fullscreen")
        .css_classes(["flat", "circular"])
        .action_name("win.toggle-fullscreen")
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
    //
    // A route ride gets the same way back, but only navigates: its pause lives
    // in the ride loop rather than in a shared engine, and the button says
    // "Back to Ride" rather than promising a resume it cannot perform. Without
    // this a rider who left a route ride — or who had fullscreen hide the page
    // out from under them — had no route back to it at all.
    let stack_for_btn = stack.clone();
    let engine_for_btn = Rc::clone(&engine_rc);
    let player_for_btn = Rc::clone(&player_rc);
    start_btn.connect_clicked(move |_| {
        if route_timer_alive.get() {
            stack_for_btn.set_visible_child_name("route_player");
            return;
        }
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

    // In fullscreen the header bar goes, and its exit button with it. This one
    // floats over the top-right corner of the page instead so there is always a
    // visible way back that does not require knowing about F11.
    let fs_float_btn = gtk::Button::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Exit Fullscreen")
        .css_classes(["osd", "circular"])
        .action_name("win.toggle-fullscreen")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_end(12)
        .visible(false)
        .build();

    let root = gtk::Overlay::new();
    root.set_child(Some(&split_view));
    root.add_overlay(&fs_float_btn);

    ContentChrome {
        root,
        split_view,
        stack: stack.clone(),
        header: content_header,
        fs_exit_btn,
        fs_float_btn,
    }
}

/// Make fullscreen mean the ride and nothing else.
///
/// Fullscreen used to be nothing but a larger window: the sidebar, the header
/// bar and the page's windowed margins all stayed, so the metrics a rider is
/// squinting at from a metre away grew barely at all. Now the frame stands down
/// — sidebar collapsed, header hidden, content clamp widened — and the two ride
/// pages lay themselves out for the room they have been given.
pub fn connect_fullscreen(
    window: &adw::ApplicationWindow,
    chrome: &ContentChrome,
    player_rc: Rc<RefCell<PlayerPage>>,
    route_player_rc: Rc<crate::ui::pages::route_player::RoutePlayerPage>,
) {
    let split_view = chrome.split_view.clone();
    let stack = chrome.stack.clone();
    let header = chrome.header.clone();
    let fs_exit_btn = chrome.fs_exit_btn.clone();
    let fs_float_btn = chrome.fs_float_btn.clone();

    // The page fullscreen was entered on, so leaving fullscreen can put it back.
    //
    // Bringing the sidebar back moves keyboard focus into it, and a focused row
    // in a single-selection GtkListBox selects itself — which fires
    // `row-selected` and navigates. The rider pressed Escape on a ride and
    // landed on whatever the sidebar happened to have highlighted, usually the
    // Library page they started the ride from. Nothing in the ride asked to be
    // left, so the ride is what comes back.
    let page_before_fullscreen: Rc<RefCell<Option<glib::GString>>> = Rc::new(RefCell::new(None));

    window.connect_fullscreened_notify(move |win| {
        let fs = win.is_fullscreen();
        // Order matters. A collapsed AdwNavigationSplitView shows whichever of
        // its two pages is on top of its navigation stack, and that is the
        // *sidebar* unless `show-content` says otherwise — so collapsing alone
        // hides the ride behind the nav list, which is the exact opposite of
        // what fullscreen is for. Ask for the content pane first, then collapse.
        if fs {
            *page_before_fullscreen.borrow_mut() = stack.visible_child_name();
            split_view.set_show_content(true);
        }
        split_view.set_collapsed(fs);
        header.set_visible(!fs);
        fs_exit_btn.set_visible(fs);
        fs_float_btn.set_visible(fs);
        player_rc.borrow().set_fullscreen(fs);
        route_player_rc.set_fullscreen(fs);

        if !fs {
            let Some(page) = page_before_fullscreen.borrow_mut().take() else {
                return;
            };
            // Deferred: the focus change that steals the selection happens as
            // the sidebar is laid back out, which is after this handler returns.
            // Restoring now would be overwritten a moment later.
            let stack = stack.clone();
            glib::idle_add_local_once(move || {
                if stack.child_by_name(&page).is_some() {
                    stack.set_visible_child_name(&page);
                }
            });
        }
    });

    // Escape leaves fullscreen, the way every full-screen view on the desktop
    // does.
    //
    // This runs in the capture phase, before anything inside the window sees the
    // key, because in fullscreen the split view is collapsed and a collapsed
    // AdwNavigationSplitView drives an AdwNavigationView underneath — which
    // treats Escape as "go back" and pops the content page off, leaving the
    // sidebar filling the screen. The first Escape did that and only the second
    // one reached this handler.
    //
    // Capturing means being careful about what else wants Escape. A dialog is
    // the one thing that legitimately does, and in fullscreen it sits inside
    // this window, so it would be captured from too — hence the check: while a
    // dialog is up, Escape belongs to the dialog.
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_ctrl.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape
                && window.is_fullscreen()
                && window.visible_dialog().is_none()
            {
                window.unfullscreen();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
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
