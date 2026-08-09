//! Saved GPX routes: the section above the workouts, and putting a route in it.

use adw::prelude::*;
use sqlx::SqlitePool;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::db;
use crate::data::route::Route;

use super::detail::show_route_detail;
use super::RebuildHolder;

/// Build the Routes section's reload closure.
///
/// The closure re-reads the saved routes and repopulates `container`. It is
/// held in a self-reference so a rename can trigger its own refresh.
pub fn reload_closure(
    routes_container: gtk::Box,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_start_route: Rc<dyn Fn(Route)>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
) -> Rc<dyn Fn()> {
    let routes_container = routes_container.clone();
    let pool = pool.clone();
    let rt_handle = rt_handle.clone();
    let on_start_route = Rc::clone(&on_start_route);
    let on_toast = Rc::clone(&on_toast);
    let holder: RebuildHolder = Rc::new(RefCell::new(None));
    let holder_outer = Rc::clone(&holder);

    let build: Rc<dyn Fn()> = Rc::new(move || {
        let routes_container = routes_container.clone();
        let pool_inner = pool.clone();
        let rt_inner = rt_handle.clone();
        let on_start_route = Rc::clone(&on_start_route);
        let on_toast = Rc::clone(&on_toast);
        let holder = Rc::clone(&holder);
        let pool_load = pool.clone();

        crate::ui::spawn_to_main(
            &rt_handle,
            async move { db::load_routes(&pool_load).await },
            move |result| {
                // A failed read leaves the previous list on screen: an
                // empty Routes section reads as "you have saved none",
                // which is a claim about the rider's library.
                let routes = match result {
                    Ok(routes) => routes,
                    Err(e) => {
                        tracing::error!("Could not load your saved routes: {e}");
                        return;
                    }
                };

                while let Some(child) = routes_container.first_child() {
                    routes_container.remove(&child);
                }
                if routes.is_empty() {
                    return; // nothing saved yet — no empty section
                }

                let group = adw::PreferencesGroup::builder()
                    .title("Routes")
                    .description("GPX routes saved to your library")
                    .build();

                for route in routes {
                    let subtitle = format!(
                        "{:.1} km · {:.0} m climb",
                        route.distance_m / 1000.0,
                        route.elevation_gain_m
                    );
                    let row = adw::ActionRow::builder()
                        .title(&route.name)
                        .subtitle(&subtitle)
                        .activatable(true)
                        .build();
                    row.add_prefix(&gtk::Image::from_icon_name("map-symbolic"));

                    let rename_btn = gtk::Button::builder()
                        .icon_name("document-edit-symbolic")
                        .tooltip_text("Rename this route")
                        .css_classes(["flat", "circular"])
                        .valign(gtk::Align::Center)
                        .build();
                    row.add_suffix(&rename_btn);

                    let pool_rename = pool_inner.clone();
                    let rt_rename = rt_inner.clone();
                    let holder_rename = Rc::clone(&holder);
                    let on_toast_rename = Rc::clone(&on_toast);
                    let route_id_rename = route.id;
                    let current_name = route.name.clone();
                    rename_btn.connect_clicked(move |btn| {
                        let dialog = adw::Dialog::builder()
                            .title("Rename Route")
                            .content_width(380)
                            .build();
                        let toolbar = adw::ToolbarView::new();
                        let header = adw::HeaderBar::new();
                        let save_btn = gtk::Button::builder()
                            .label("Save")
                            .css_classes(["suggested-action"])
                            .build();
                        header.pack_end(&save_btn);
                        toolbar.add_top_bar(&header);

                        let group = adw::PreferencesGroup::builder()
                            .margin_top(18)
                            .margin_bottom(18)
                            .margin_start(18)
                            .margin_end(18)
                            .build();
                        let entry = adw::EntryRow::builder().title("Name").build();
                        entry.set_text(&current_name);
                        group.add(&entry);
                        toolbar.set_content(Some(&group));
                        dialog.set_child(Some(&toolbar));

                        // Saving from the button and pressing Enter in the
                        // field do the same thing.
                        let commit: Rc<dyn Fn(&adw::EntryRow)> = {
                            let pool = pool_rename.clone();
                            let rt = rt_rename.clone();
                            let holder = Rc::clone(&holder_rename);
                            let on_toast = Rc::clone(&on_toast_rename);
                            // Weak: this closure is reached from save_btn and
                            // from the entry row, both inside the dialog, so a
                            // strong capture would be a cycle (CLAUDE.md §2.4).
                            Rc::new(glib::clone!(
                                #[weak]
                                dialog,
                                move |entry: &adw::EntryRow| {
                                    let name = entry.text().to_string();
                                    if name.trim().is_empty() {
                                        on_toast(
                                            adw::Toast::builder()
                                                .title("A route needs a name")
                                                .timeout(3)
                                                .build(),
                                        );
                                        return;
                                    }
                                    let pool = pool.clone();
                                    let holder = Rc::clone(&holder);
                                    let on_toast = Rc::clone(&on_toast);
                                    dialog.close();
                                    crate::ui::spawn_to_main(
                                        &rt,
                                        async move {
                                            db::rename_route(&pool, route_id_rename, &name).await
                                        },
                                        move |result| {
                                            if let Err(e) = result {
                                                tracing::error!("rename_route failed: {e}");
                                                on_toast(
                                                    adw::Toast::builder()
                                                        .title("Could not rename that route")
                                                        .timeout(4)
                                                        .build(),
                                                );
                                                return;
                                            }
                                            if let Some(reload) = holder.borrow().as_ref() {
                                                reload();
                                            }
                                        },
                                    );
                                }
                            ))
                        };

                        let commit_btn = Rc::clone(&commit);
                        let entry_btn = entry.clone();
                        save_btn.connect_clicked(move |_| commit_btn(&entry_btn));
                        let commit_entry = Rc::clone(&commit);
                        entry.connect_apply(move |e| commit_entry(e));

                        dialog.present(Some(btn));
                    });

                    let delete_btn = gtk::Button::builder()
                        .icon_name("user-trash-symbolic")
                        .tooltip_text("Remove this route from the library")
                        .css_classes(["flat", "circular"])
                        .valign(gtk::Align::Center)
                        .build();
                    row.add_suffix(&delete_btn);

                    // Open: parse the saved file off the main thread, then
                    // show the same detail dialog a freshly loaded GPX gets.
                    let pool_open = pool_inner.clone();
                    let rt_open = rt_inner.clone();
                    let on_start_open = Rc::clone(&on_start_route);
                    let on_toast_open = Rc::clone(&on_toast);
                    let file_name = route.file_name.clone();
                    row.connect_activated(move |row| {
                        let parent = row.root().and_downcast::<gtk::Window>();
                        let on_start = Rc::clone(&on_start_open);
                        let on_toast = Rc::clone(&on_toast_open);
                        let file_name = file_name.clone();
                        let _ = &pool_open;
                        crate::ui::spawn_to_main(
                            &rt_open,
                            async move {
                                let dir = db::routes_dir()?;
                                Route::from_gpx_path(&dir.join(&file_name))
                            },
                            move |parsed| match parsed {
                                Ok(route) => {
                                    show_route_detail(route, parent.as_ref(), Rc::clone(&on_start))
                                }
                                Err(e) => {
                                    tracing::error!("Failed to read saved route: {e}");
                                    on_toast(
                                        adw::Toast::builder()
                                            .title("Could not open that route")
                                            .timeout(4)
                                            .build(),
                                    );
                                }
                            },
                        );
                    });

                    let pool_del = pool_inner.clone();
                    let rt_del = rt_inner.clone();
                    let holder_del = Rc::clone(&holder);
                    let on_toast_del = Rc::clone(&on_toast);
                    let route_id = route.id;
                    let route_name = route.name.clone();
                    delete_btn.connect_clicked(move |btn| {
                        let dialog = adw::AlertDialog::builder()
                            .heading("Remove Route?")
                            .body(format!(
                                "\u{201c}{route_name}\u{201d} will be removed from your \
                                     library and its GPX file deleted."
                            ))
                            .build();
                        dialog.add_response("cancel", "_Cancel");
                        dialog.add_response("remove", "_Remove");
                        dialog.set_response_appearance(
                            "remove",
                            adw::ResponseAppearance::Destructive,
                        );
                        dialog.set_default_response(Some("cancel"));
                        dialog.set_close_response("cancel");

                        let pool = pool_del.clone();
                        let rt = rt_del.clone();
                        let holder = Rc::clone(&holder_del);
                        let on_toast = Rc::clone(&on_toast_del);
                        dialog.connect_response(None, move |_, response| {
                            if response != "remove" {
                                return;
                            }
                            let pool = pool.clone();
                            let holder = Rc::clone(&holder);
                            let on_toast = Rc::clone(&on_toast);
                            crate::ui::spawn_to_main(
                                &rt,
                                async move { db::delete_route(&pool, route_id).await },
                                move |result| {
                                    if let Err(e) = result {
                                        tracing::error!("delete_route failed: {e}");
                                        on_toast(
                                            adw::Toast::builder()
                                                .title("Could not remove that route")
                                                .timeout(4)
                                                .build(),
                                        );
                                        return;
                                    }
                                    if let Some(reload) = holder.borrow().as_ref() {
                                        reload();
                                    }
                                },
                            );
                        });
                        dialog.present(Some(btn));
                    });

                    group.add(&row);
                }
                routes_container.append(&group);
            },
        );
    });

    *holder_outer.borrow_mut() = Some(Rc::clone(&build));
    build
}

/// Copy a loaded GPX into the library directory and record it, so the route can
/// be ridden again without hunting for the original file.
///
/// The stored file name is derived from the route name and made unique with a
/// timestamp, so importing two files called `route.gpx` does not have one
/// silently replace the other.
pub fn save_route_to_library(
    source: &std::path::Path,
    route: &Route,
    pool: SqlitePool,
    rt: tokio::runtime::Handle,
    reload: Rc<dyn Fn()>,
    on_toast: Rc<dyn Fn(adw::Toast)>,
) {
    let stem: String = route
        .name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_');
    let stem = if stem.is_empty() { "route" } else { stem };
    let file_name = format!("{stem}-{}.gpx", chrono::Utc::now().format("%Y%m%d%H%M%S"));

    let dir = match db::routes_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("routes directory unavailable: {e}");
            on_toast(
                adw::Toast::builder()
                    .title("Could not save the route")
                    .timeout(4)
                    .build(),
            );
            return;
        }
    };
    if let Err(e) = std::fs::copy(source, dir.join(&file_name)) {
        tracing::error!("copying GPX into the library failed: {e}");
        on_toast(
            adw::Toast::builder()
                .title("Could not save the route")
                .timeout(4)
                .build(),
        );
        return;
    }

    let name = route.name.clone();
    let distance_m = route.total_distance_m;
    let gain_m = route.total_gain_m;
    crate::ui::spawn_to_main(
        &rt,
        async move { db::save_route(&pool, &name, &file_name, distance_m, gain_m).await },
        move |result| match result {
            Ok(_) => {
                reload();
                on_toast(
                    adw::Toast::builder()
                        .title("Route saved to your library")
                        .timeout(4)
                        .build(),
                );
            }
            Err(e) => {
                tracing::error!("save_route failed: {e}");
                on_toast(
                    adw::Toast::builder()
                        .title("Could not save the route")
                        .timeout(4)
                        .build(),
                );
            }
        },
    );
}
