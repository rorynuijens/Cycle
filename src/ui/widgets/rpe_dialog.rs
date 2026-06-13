use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::ui::resources::rpe_texture;

// (rpe, css_color_class, label)
const RPE_LEVELS: [(u8, &str, &str); 6] = [
    (1, "success", "Very Easy"),
    (2, "accent", "Easy"),
    (3, "accent", "Moderate"),
    (4, "warning", "Hard"),
    (5, "error", "Very Hard"),
    (6, "error", "Maximum Effort"),
];

/// Show a post-workout RPE dialog over `parent`.
///
/// `on_submit` is called with the selected RPE value (1–10) when the user
/// confirms. If the user skips, `on_submit` is not called.
pub fn show(parent: &impl IsA<gtk::Widget>, on_submit: impl Fn(u8) + 'static) {
    let selected: Rc<Cell<u8>> = Rc::new(Cell::new(0));

    // ── Content layout ────────────────────────────────────────────────────
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .width_request(340)
        .build();

    let subtitle = gtk::Label::builder()
        .label("How hard did the workout feel?")
        .css_classes(["dim-label"])
        .halign(gtk::Align::Center)
        .wrap(true)
        .build();
    content.append(&subtitle);

    // ── Emoji button grid: 5 per row ──────────────────────────────────────
    let grid = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();

    let row1 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let row2 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();

    let feedback = gtk::Label::builder()
        .label("No rating selected")
        .css_classes(["caption", "dim-label"])
        .halign(gtk::Align::Center)
        .build();

    // ── Save button — created early so RPE buttons can enable it ────────────
    let save_btn = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action", "pill"])
        .tooltip_text("Save RPE rating")
        .sensitive(false)
        .build();

    let buttons: Vec<gtk::Button> = RPE_LEVELS
        .iter()
        .map(|(rpe, color_class, label)| {
            let vbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build();

            // Show bundled resource icon, falling back to a coloured number label.
            if let Some(texture) = rpe_texture(*rpe) {
                let image = gtk::Image::builder()
                    .paintable(&texture)
                    .pixel_size(50)
                    .halign(gtk::Align::Center)
                    .build();
                vbox.append(&image);
            } else {
                let num_lbl = gtk::Label::builder()
                    .label(rpe.to_string())
                    .css_classes(["title-1", *color_class])
                    .halign(gtk::Align::Center)
                    .build();
                vbox.append(&num_lbl);
            }

            let name_lbl = gtk::Label::builder()
                .label(*label)
                .css_classes(["caption", "dim-label"])
                .halign(gtk::Align::Center)
                .build();
            vbox.append(&name_lbl);

            let btn = gtk::Button::builder()
                .css_classes(["flat"])
                .width_request(72)
                .height_request(78)
                .tooltip_text(*label)
                .build();
            btn.set_child(Some(&vbox));
            btn
        })
        .collect();

    // Wire each button: highlight selected, update feedback label, enable Save
    for (i, (rpe, _color_class, label)) in RPE_LEVELS.iter().enumerate() {
        let rpe = *rpe;
        let label = label.to_string();
        let sel = Rc::clone(&selected);
        let feedback_ref = feedback.clone();
        let buttons_ref = buttons.clone();
        let save_ref = save_btn.clone();

        buttons[i].connect_clicked(move |_| {
            sel.set(rpe);
            save_ref.set_sensitive(true);
            feedback_ref.set_label(&format!("{rpe} — {label}"));
            for (j, btn) in buttons_ref.iter().enumerate() {
                if j + 1 == rpe as usize {
                    btn.set_css_classes(&["suggested-action"]);
                } else {
                    btn.set_css_classes(&["flat"]);
                }
            }
        });

        if i < 3 {
            row1.append(&buttons[i]);
        } else {
            row2.append(&buttons[i]);
        }
    }

    grid.append(&row1);
    grid.append(&row2);
    content.append(&grid);
    content.append(&feedback);

    // ── Action row ────────────────────────────────────────────────────────
    let btn_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();

    let skip_btn = gtk::Button::builder()
        .label("Skip")
        .css_classes(["flat", "pill"])
        .tooltip_text("Skip RPE rating")
        .build();

    btn_row.append(&skip_btn);
    btn_row.append(&save_btn);
    content.append(&btn_row);

    // ── Dialog wrapper ────────────────────────────────────────────────────
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&content));

    let dialog = adw::Dialog::builder()
        .title("Rate Your Effort")
        .child(&toolbar_view)
        .build();

    let dialog_skip = dialog.clone();
    skip_btn.connect_clicked(move |_| {
        dialog_skip.close();
    });

    let dialog_save = dialog.clone();
    let on_submit = Rc::new(on_submit);
    save_btn.connect_clicked(move |_| {
        let rpe = selected.get();
        if rpe > 0 {
            on_submit(rpe);
        }
        dialog_save.close();
    });

    dialog.present(Some(parent));
}
