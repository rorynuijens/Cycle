//! The rider's training goals — what the coach is aiming them at.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::db;

/// Set after construction, because the page's reload closure is built from the
/// sections and so cannot exist while they are being made.
type ReloadHolder = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

pub struct GoalsSection {
    root: gtk::Box,
    list: gtk::ListBox,
    empty_label: gtk::Label,
    reload: ReloadHolder,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
}

impl GoalsSection {
    pub fn new(pool: SqlitePool, rt_handle: tokio::runtime::Handle) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        header.append(
            &gtk::Label::builder()
                .label("Goals")
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .tooltip_text(
                    "Goals help the AI Coach give more targeted workout suggestions \
                     and training programs",
                )
                .build(),
        );
        let add_btn = gtk::Button::builder()
            .label("Add Goal")
            .css_classes(["pill"])
            .tooltip_text("Add a training goal")
            .halign(gtk::Align::End)
            .build();
        header.append(&add_btn);
        root.append(&header);

        let list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        root.append(&list);

        let empty_label = gtk::Label::builder()
            .label(
                "No training goals added yet. Goals help the AI Coach give more targeted \
                 workout suggestions and training programs.",
            )
            .css_classes(["dim-label"])
            .halign(gtk::Align::Center)
            .wrap(true)
            .visible(false)
            .build();
        root.append(&empty_label);

        let reload: ReloadHolder = Rc::new(RefCell::new(None));
        Self::connect_add(&add_btn, &pool, &rt_handle, &reload);

        Self {
            root,
            list,
            empty_label,
            reload,
            pool,
            rt_handle,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Hand the section the page reload, so adding or removing a goal can
    /// refresh the list it came from.
    pub fn set_reload(&self, reload: Rc<dyn Fn()>) {
        *self.reload.borrow_mut() = Some(reload);
    }

    /// Rebuild the list. With no goals the list gives way to an explanation —
    /// an empty boxed list would read as a rendering failure.
    pub fn set_goals(&self, goals: &[db::AthleteGoal]) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }

        self.list.set_visible(!goals.is_empty());
        self.empty_label.set_visible(goals.is_empty());

        for goal in goals {
            let row = adw::ActionRow::builder().title(&goal.description).build();
            row.add_suffix(&self.delete_button(goal.id));
            self.list.append(&row);
        }
    }

    /// The trash button on one goal's row.
    fn delete_button(&self, goal_id: i64) -> gtk::Button {
        let button = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Remove this goal")
            .valign(gtk::Align::Center)
            .build();

        let pool = self.pool.clone();
        let rt_handle = self.rt_handle.clone();
        let reload = Rc::clone(&self.reload);
        button.connect_clicked(move |_| {
            let pool = pool.clone();
            let reload = Rc::clone(&reload);
            crate::ui::spawn_to_main(
                &rt_handle,
                async move { db::delete_goal(&pool, goal_id).await },
                move |res| match res {
                    Ok(()) => {
                        if let Some(reload) = reload.borrow().as_ref() {
                            reload();
                        }
                    }
                    Err(e) => tracing::error!("delete_goal failed: {e}"),
                },
            );
        });

        button
    }

    fn connect_add(
        button: &gtk::Button,
        pool: &SqlitePool,
        rt_handle: &tokio::runtime::Handle,
        reload: &ReloadHolder,
    ) {
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        let reload = Rc::clone(reload);

        button.connect_clicked(move |btn| {
            let dialog = adw::AlertDialog::new(Some("Add Training Goal"), None::<&str>);
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("add", "Add");
            dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("add"));
            dialog.set_close_response("cancel");

            let entry = gtk::Entry::builder()
                .placeholder_text("e.g. Complete a 100 km sportive by September")
                .hexpand(true)
                .activates_default(true)
                .build();
            dialog.set_extra_child(Some(&entry));

            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let reload = Rc::clone(&reload);
            let entry_for_response = entry.clone();
            dialog.connect_response(None, move |_, response| {
                if response != "add" {
                    return;
                }
                let description = entry_for_response.text().trim().to_string();
                if description.is_empty() {
                    return;
                }

                let pool = pool.clone();
                let reload = Rc::clone(&reload);
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move { db::save_goal(&pool, &description).await },
                    move |res| match res {
                        Ok(_) => {
                            if let Some(reload) = reload.borrow().as_ref() {
                                reload();
                            }
                        }
                        Err(e) => tracing::error!("save_goal failed: {e}"),
                    },
                );
            });

            dialog.present(Some(btn));
            entry.grab_focus();
        });
    }
}
