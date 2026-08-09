//! The Athlete page: who the rider is, and the numbers everything else scales to.
//!
//! Changes apply immediately — there is no Save button — so every row writes
//! through on change and reports the new profile to the rest of the app.

use adw::prelude::*;
use gtk::glib;
use sqlx::SqlitePool;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use crate::data::settings;
use crate::data::{athlete::AthleteProfile, db};

/// How long an FTP edit must settle before it is written to the FTP history.
///
/// Clicking a spinner from 250 to 260 fires ten times; the history wants the
/// value the rider stopped on, not every notch on the way.
const FTP_LOG_DEBOUNCE: Duration = Duration::from_secs(3);

/// Resting heart rate is kept this far below maximum, whatever is typed —
/// the zone maths divides by the reserve between them.
const MIN_HR_RESERVE: u32 = 10;

/// First line of the training context, truncated, for the row subtitle.
fn context_preview(ctx: &str) -> String {
    let first = ctx.trim().lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "Not set — the AI Coach gives better advice with context".to_string();
    }
    let truncated: String = first.chars().take(72).collect();
    if truncated.len() < first.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Build the Athlete page and wire it to write through on every change.
pub fn build(
    win: &adw::PreferencesWindow,
    athlete: AthleteProfile,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_saved: Rc<dyn Fn(AthleteProfile)>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Athlete")
        .icon_name("avatar-default-symbolic")
        .build();

    let identity_group = adw::PreferencesGroup::builder().title("Identity").build();
    let name_row = adw::EntryRow::builder().title("Name").build();
    name_row.set_text(&athlete.name);
    identity_group.add(&name_row);
    page.add(&identity_group);

    let perf_group = adw::PreferencesGroup::builder()
        .title("Performance")
        .build();
    let ftp_adj = gtk::Adjustment::new(athlete.ftp_watts as f64, 50.0, 2000.0, 1.0, 10.0, 0.0);
    let ftp_row = adw::SpinRow::new(Some(&ftp_adj), 1.0, 0);
    ftp_row.set_title("FTP");
    ftp_row.set_subtitle("Functional Threshold Power (watts)");
    perf_group.add(&ftp_row);

    let weight_adj = gtk::Adjustment::new(athlete.weight_kg as f64, 30.0, 200.0, 0.5, 5.0, 0.0);
    let weight_row = adw::SpinRow::new(Some(&weight_adj), 1.0, 1);
    weight_row.set_title("Weight");
    weight_row.set_subtitle("Body weight (kg)");
    perf_group.add(&weight_row);
    page.add(&perf_group);

    let hr_group = adw::PreferencesGroup::builder().title("Heart Rate").build();
    let max_hr_adj = gtk::Adjustment::new(athlete.max_hr as f64, 100.0, 250.0, 1.0, 5.0, 0.0);
    let max_hr_row = adw::SpinRow::new(Some(&max_hr_adj), 1.0, 0);
    max_hr_row.set_title("Maximum HR");
    max_hr_row.set_subtitle("Maximum heart rate (bpm)");
    hr_group.add(&max_hr_row);

    let resting_hr_adj =
        gtk::Adjustment::new(athlete.resting_hr as f64, 30.0, 120.0, 1.0, 5.0, 0.0);
    let resting_hr_row = adw::SpinRow::new(Some(&resting_hr_adj), 1.0, 0);
    resting_hr_row.set_title("Resting HR");
    resting_hr_row.set_subtitle("Resting heart rate (bpm)");
    hr_group.add(&resting_hr_row);
    page.add(&hr_group);

    let coaching_group = adw::PreferencesGroup::builder()
        .title("Coaching")
        .description("Context the AI Coach reads with every request.")
        .build();
    let context_row = adw::ActionRow::builder()
        .title("Training Context")
        .subtitle("Loading…")
        .use_markup(false)
        .activatable(true)
        .tooltip_text(
            "Describe your age, lifestyle, time constraints, and training preferences — \
             the more detail, the more personalised the coaching",
        )
        .build();
    context_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    coaching_group.add(&context_row);
    page.add(&coaching_group);

    let danger_group = adw::PreferencesGroup::builder()
        .title("Danger Zone")
        .description("These actions are permanent and cannot be undone.")
        .build();
    let delete_row = adw::ActionRow::builder()
        .title("Delete Athlete Profile")
        .subtitle("Reset your profile to defaults. API keys are not affected.")
        .build();
    let delete_btn = gtk::Button::builder()
        .label("Delete…")
        .css_classes(["destructive-action", "pill"])
        .tooltip_text("Permanently delete the athlete profile")
        .valign(gtk::Align::Center)
        .build();
    delete_row.add_suffix(&delete_btn);
    danger_group.add(&delete_row);
    page.add(&danger_group);

    // ── Live apply ────────────────────────────────────────────────────────
    let athlete_id = athlete.id;
    let original_name = athlete.name.clone();
    let apply: Rc<dyn Fn()> = {
        let (name_row, ftp_row, weight_row) =
            (name_row.clone(), ftp_row.clone(), weight_row.clone());
        let (max_hr_row, resting_hr_row) = (max_hr_row.clone(), resting_hr_row.clone());
        let on_saved = Rc::clone(&on_saved);
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();

        Rc::new(move || {
            let typed_name = name_row.text().to_string();
            let max_hr = max_hr_row.value() as u32;
            let profile = AthleteProfile {
                id: athlete_id,
                // An empty name is a half-finished edit, not a request to be
                // anonymous — keep the one they had.
                name: if typed_name.trim().is_empty() {
                    original_name.clone()
                } else {
                    typed_name.trim().to_string()
                },
                ftp_watts: ftp_row.value() as u32,
                weight_kg: weight_row.value() as f32,
                max_hr,
                resting_hr: (resting_hr_row.value() as u32)
                    .min(max_hr.saturating_sub(MIN_HR_RESERVE)),
            };

            on_saved(profile.clone());

            let pool = pool.clone();
            rt_handle.spawn(async move {
                if let Err(e) = db::update_athlete(&pool, &profile).await {
                    tracing::error!("update_athlete failed: {e}");
                }
            });
        })
    };

    {
        let apply = Rc::clone(&apply);
        name_row.connect_apply(move |_| apply());
    }
    for row in [&weight_row, &max_hr_row, &resting_hr_row] {
        let apply = Rc::clone(&apply);
        row.connect_value_notify(move |_| apply());
    }
    connect_ftp(&ftp_row, apply, athlete.ftp_watts, &pool, &rt_handle);

    connect_context(&context_row, win, &pool, &rt_handle);
    connect_delete(&delete_btn, win, &pool, &rt_handle, on_saved);

    page
}

/// FTP writes through like the other rows, and additionally logs to the FTP
/// history that drives detection (docs/ftp-detection.md).
fn connect_ftp(
    ftp_row: &adw::SpinRow,
    apply: Rc<dyn Fn()>,
    initial_ftp: u32,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    let generation = Rc::new(Cell::new(0u32));
    let last_logged = Rc::new(Cell::new(initial_ftp));
    let pool = pool.clone();
    let rt_handle = rt_handle.clone();

    ftp_row.connect_value_notify(move |row| {
        apply();

        // Each change starts a new countdown and invalidates the ones before
        // it, so only the value the rider settled on reaches the history.
        let this_generation = generation.get().wrapping_add(1);
        generation.set(this_generation);

        let row = row.clone();
        let generation = Rc::clone(&generation);
        let last_logged = Rc::clone(&last_logged);
        let pool = pool.clone();
        let rt_handle = rt_handle.clone();
        glib::timeout_add_local_once(FTP_LOG_DEBOUNCE, move || {
            if generation.get() != this_generation {
                return; // superseded by a newer change
            }
            let ftp = row.value() as u32;
            if ftp == last_logged.get() {
                return;
            }
            last_logged.set(ftp);
            rt_handle.spawn(async move {
                if let Err(e) = db::log_ftp_change(&pool, ftp, "manual", "").await {
                    tracing::error!("log_ftp_change failed: {e}");
                }
            });
        });
    });
}

/// Fill the context row's preview, and open the editor when it is activated.
fn connect_context(
    context_row: &adw::ActionRow,
    win: &adw::PreferencesWindow,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
) {
    // Populate the preview off the main thread (CLAUDE.md §2.3).
    let row = context_row.clone();
    let pool_preview = pool.clone();
    crate::ui::spawn_to_main(
        rt_handle,
        async move {
            settings::coaching_context(&pool_preview)
                .await
                .unwrap_or_default()
        },
        move |ctx| row.set_subtitle(&context_preview(&ctx)),
    );

    let pool = pool.clone();
    let rt_handle = rt_handle.clone();
    // Weak: context_row sits inside this window (CLAUDE.md §2.4).
    context_row.connect_activated(glib::clone!(
        #[weak]
        win,
        move |row| {
            show_context_editor(&win, pool.clone(), rt_handle.clone(), row.clone());
        }
    ));
}

fn connect_delete(
    delete_btn: &gtk::Button,
    win: &adw::PreferencesWindow,
    pool: &SqlitePool,
    rt_handle: &tokio::runtime::Handle,
    on_saved: Rc<dyn Fn(AthleteProfile)>,
) {
    let pool = pool.clone();
    let rt_handle = rt_handle.clone();

    // Weak: delete_btn sits inside this window (CLAUDE.md §2.4).
    delete_btn.connect_clicked(glib::clone!(
        #[weak]
        win,
        move |btn| {
            let dialog = adw::AlertDialog::builder()
                .heading("Delete Athlete Profile?")
                .body(
                    "Your athlete profile (name, FTP, weight, heart rate) will be reset \
                 to defaults. This cannot be undone.\n\n\
                 API keys and device settings are always preserved.",
                )
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("delete", "_Delete");
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let wipe_check = gtk::CheckButton::builder()
                .label(
                    "Also delete all training data\n\
                 (sessions, workouts, calendar, activities, wellness, goals, time off)",
                )
                .active(false)
                .margin_top(6)
                .build();
            dialog.set_extra_child(Some(&wipe_check));

            let pool = pool.clone();
            let rt_handle = rt_handle.clone();
            let on_saved = Rc::clone(&on_saved);
            let win = win.clone();
            dialog.connect_response(None, move |_, response| {
                if response != "delete" {
                    return;
                }
                let wipe_data = wipe_check.is_active();
                let pool = pool.clone();
                let on_saved = Rc::clone(&on_saved);
                let win = win.clone();
                crate::ui::spawn_to_main(
                    &rt_handle,
                    async move {
                        db::reset_athlete_data(&pool, wipe_data)
                            .await
                            .map_err(|e| tracing::error!("reset_athlete_data failed: {e}"))?;
                        // Recreate a default athlete so the live engine stays valid.
                        Ok(db::load_or_create_athlete(&pool).await.unwrap_or_default())
                    },
                    move |res: Result<AthleteProfile, ()>| match res {
                        Err(()) => win.add_toast(
                            adw::Toast::builder()
                                .title("Failed to delete profile")
                                .timeout(4)
                                .build(),
                        ),
                        Ok(default_athlete) => {
                            on_saved(default_athlete);
                            win.add_toast(
                                adw::Toast::builder()
                                    .title(if wipe_data {
                                        "Profile and all training data deleted"
                                    } else {
                                        "Athlete profile reset to defaults"
                                    })
                                    .timeout(4)
                                    .build(),
                            );
                            win.close();
                        }
                    },
                );
            });

            dialog.present(Some(btn));
        }
    ));
}

/// Present the training-context editor, saving to `coaching.athlete_context`
/// and refreshing `preview_row`'s subtitle.
fn show_context_editor(
    win: &adw::PreferencesWindow,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    preview_row: adw::ActionRow,
) {
    let win = win.clone();
    let pool_load = pool.clone();
    let rt_save = rt_handle.clone();
    // Load the current context off the main thread (CLAUDE.md §2.3), then
    // build and present the dialog when it arrives.
    crate::ui::spawn_to_main(
        &rt_handle,
        async move {
            settings::coaching_context(&pool_load)
                .await
                .unwrap_or_default()
        },
        move |current| {
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .margin_top(12)
                .margin_bottom(24)
                .margin_start(24)
                .margin_end(24)
                .build();

            content.append(
                &gtk::Label::builder()
                    .label(
                        "Describe your age, lifestyle, time constraints, and training \
                         preferences. The AI Coach uses this in every coaching response.",
                    )
                    .css_classes(["dim-label"])
                    .halign(gtk::Align::Start)
                    .wrap(true)
                    .build(),
            );

            let template_btn = gtk::Button::builder()
                .label("Use template")
                .css_classes(["pill"])
                .tooltip_text("Fill in a starter template")
                .halign(gtk::Align::Start)
                .build();
            content.append(&template_btn);

            let text_view = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::Word)
                .accepts_tab(false)
                .hexpand(true)
                .build();
            text_view.buffer().set_text(&current);

            let tv_scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .min_content_height(120)
                .hexpand(true)
                .child(&text_view)
                .build();
            let tv_frame = gtk::Box::builder()
                .css_classes(["card"])
                .orientation(gtk::Orientation::Vertical)
                .build();
            tv_frame.append(&tv_scroll);
            content.append(&tv_frame);

            let tv_for_template = text_view.clone();
            template_btn.connect_clicked(move |_| {
                tv_for_template.buffer().set_text(
                    "I am [AGE] years old [GENDER]. [DESCRIBE YOUR LIFESTYLE AND \
                     TIME CONSTRAINTS].\nMy training goals are: [LIST YOUR GOALS].\n\
                     I prefer workouts that are [PREFERENCES — e.g. time-efficient, \
                     varied, low-impact].\nAdditional notes: [ANYTHING ELSE].",
                );
            });

            let header = adw::HeaderBar::new();
            let cancel_btn = gtk::Button::builder()
                .label("Cancel")
                .tooltip_text("Discard changes")
                .build();
            let save_btn = gtk::Button::builder()
                .label("Save")
                .css_classes(["suggested-action"])
                .tooltip_text("Save training context")
                .build();
            header.pack_start(&cancel_btn);
            header.pack_end(&save_btn);

            let toolbar_view = adw::ToolbarView::new();
            toolbar_view.add_top_bar(&header);
            toolbar_view.set_content(Some(&content));

            let dialog = adw::Dialog::builder()
                .title("Training Context")
                .child(&toolbar_view)
                .content_width(560)
                .build();

            // Weak: both buttons are inside the dialog (CLAUDE.md §2.4). Once the
            // dialog can be freed on close, the strong `win_save` below goes with
            // it, so that one does not need weakening too.
            cancel_btn.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    dialog.close();
                }
            ));

            let win_save = win.clone();
            save_btn.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    let buffer = text_view.buffer();
                    let text = buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), false)
                        .trim()
                        .to_string();

                    let ctx = text.clone();
                    crate::ui::spawn_write(
                        &rt_save,
                        &pool,
                        "your training context",
                        |pool| async move { settings::set_coaching_context(&pool, &ctx).await },
                    );

                    preview_row.set_subtitle(&context_preview(&text));
                    win_save.add_toast(
                        adw::Toast::builder()
                            .title("Training context saved")
                            .timeout(3)
                            .build(),
                    );
                    dialog.close();
                }
            ));

            dialog.present(Some(&win));
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_prompt_for_context_when_none_is_set() {
        assert!(context_preview("").starts_with("Not set"));
        assert!(context_preview("   \n  ").starts_with("Not set"));
    }

    #[test]
    fn should_preview_only_the_first_line() {
        let preview = context_preview("I am 41 years old.\nI ride four days a week.");
        assert_eq!(preview, "I am 41 years old.");
    }

    #[test]
    fn should_truncate_a_long_first_line() {
        let preview = context_preview(&"a".repeat(200));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 73); // 72 + the ellipsis
    }

    #[test]
    fn should_not_truncate_a_line_that_fits() {
        let line = "I ride four days a week.";
        assert_eq!(context_preview(line), line);
    }

    #[test]
    fn should_cut_a_long_line_on_a_character_boundary() {
        // Byte-slicing 72 bytes into this would land mid-character.
        let preview = context_preview(&"ş".repeat(100));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 73);
    }
}
