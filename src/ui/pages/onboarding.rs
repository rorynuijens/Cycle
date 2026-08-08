use adw::prelude::*;
use sqlx::SqlitePool;
use std::cell::Cell;
use std::rc::Rc;

use crate::data::settings;
use crate::data::{db, keystore};

/// Show the first-use setup wizard as a modal `adw::Dialog`.
///
/// `on_complete` is called on the GTK main thread after the user finishes
/// all three steps and the `first_use_complete` flag is written to the DB.
pub fn show(
    parent: Option<&impl IsA<gtk::Widget>>,
    pool: SqlitePool,
    rt_handle: tokio::runtime::Handle,
    on_complete: Rc<dyn Fn()>,
) {
    let nav_view = adw::NavigationView::new();

    let dialog = adw::Dialog::builder()
        .title("Welcome to Cycle")
        .content_width(480)
        .build();
    dialog.set_child(Some(&nav_view));

    // ── Step 1: Athlete profile ───────────────────────────────────────────

    let profile_page_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let profile_header = adw::HeaderBar::new();
    profile_page_box.append(&profile_header);

    let profile_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let profile_clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    let profile_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    let welcome_label = gtk::Label::builder()
        .label("Let's get you set up. This takes about 2 minutes.")
        .halign(gtk::Align::Start)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    profile_content.append(&welcome_label);

    // Load existing athlete row so we can pre-populate and save back correctly.
    let athlete = rt_handle
        .block_on(db::load_or_create_athlete(&pool))
        .unwrap_or_default();
    let athlete_id = Rc::new(Cell::new(athlete.id));

    // Identity group
    let identity_group = adw::PreferencesGroup::builder()
        .title("Your Profile")
        .build();

    let name_row = adw::EntryRow::builder().title("Your name").build();
    name_row.set_text(&athlete.name);
    identity_group.add(&name_row);

    // Performance group
    let perf_group = adw::PreferencesGroup::builder()
        .title("Performance")
        .description("You can update these any time in Preferences.")
        .build();

    let ftp_adj = gtk::Adjustment::new(athlete.ftp_watts as f64, 50.0, 2000.0, 1.0, 10.0, 0.0);
    let ftp_row = adw::SpinRow::new(Some(&ftp_adj), 1.0, 0);
    ftp_row.set_title("FTP");
    ftp_row.set_subtitle("Functional Threshold Power in watts");
    ftp_row.set_tooltip_text(Some(
        "Your FTP is the maximum power you can sustain for one hour",
    ));
    perf_group.add(&ftp_row);

    let weight_adj = gtk::Adjustment::new(athlete.weight_kg as f64, 30.0, 200.0, 0.5, 5.0, 0.0);
    let weight_row = adw::SpinRow::new(Some(&weight_adj), 1.0, 1);
    weight_row.set_title("Weight");
    weight_row.set_subtitle("Body weight in kg");
    weight_row.set_tooltip_text(Some("Used to calculate watts per kilogram"));
    perf_group.add(&weight_row);

    let hr_adj = gtk::Adjustment::new(athlete.max_hr as f64, 100.0, 250.0, 1.0, 5.0, 0.0);
    let hr_row = adw::SpinRow::new(Some(&hr_adj), 1.0, 0);
    hr_row.set_title("Max Heart Rate");
    hr_row.set_subtitle("Beats per minute");
    hr_row.set_tooltip_text(Some("Used to calculate heart rate zones"));
    perf_group.add(&hr_row);

    profile_content.append(&identity_group);
    profile_content.append(&perf_group);

    let next_profile_btn = gtk::Button::builder()
        .label("Next")
        .css_classes(["pill", "suggested-action"])
        .tooltip_text("Continue to Intervals.icu setup")
        .build();
    profile_content.append(&next_profile_btn);

    profile_clamp.set_child(Some(&profile_content));
    profile_scroll.set_child(Some(&profile_clamp));
    profile_page_box.append(&profile_scroll);

    let profile_nav_page = adw::NavigationPage::builder()
        .title("Your Profile")
        .tag("profile")
        .child(&profile_page_box)
        .build();

    // ── Step 2: Intervals.icu ─────────────────────────────────────────────

    let icu_page_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let icu_header = adw::HeaderBar::new();
    icu_page_box.append(&icu_header);

    let icu_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let icu_clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    let icu_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    let icu_desc = gtk::Label::builder()
        .label(
            "Connect Intervals.icu to sync your training history and wellness data. \
             This is optional — you can skip this step.",
        )
        .halign(gtk::Align::Start)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    icu_content.append(&icu_desc);

    let icu_group = adw::PreferencesGroup::builder()
        .title("Intervals.icu")
        .build();

    let icu_key_row = adw::PasswordEntryRow::builder().title("API Key").build();
    let icu_id_row = adw::EntryRow::builder()
        .title("Athlete ID")
        .show_apply_button(false)
        .build();

    // Pre-populate
    if let Ok(Some(v)) = keystore::get_secret(keystore::KEY_INTERVALS_API) {
        icu_key_row.set_text(&v);
    }
    if let Ok(intervals) = rt_handle.block_on(settings::load_intervals(&pool)) {
        icu_id_row.set_text(&intervals.athlete_id);
    }

    icu_group.add(&icu_key_row);
    icu_group.add(&icu_id_row);
    icu_content.append(&icu_group);

    let icu_btn_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build();

    let skip_icu_btn = gtk::Button::builder()
        .label("Skip")
        .css_classes(["pill"])
        .tooltip_text("Skip Intervals.icu setup for now")
        .build();
    let next_icu_btn = gtk::Button::builder()
        .label("Next")
        .css_classes(["pill", "suggested-action"])
        .tooltip_text("Continue to AI provider setup")
        .build();
    icu_btn_row.append(&skip_icu_btn);
    icu_btn_row.append(&next_icu_btn);
    icu_content.append(&icu_btn_row);

    icu_clamp.set_child(Some(&icu_content));
    icu_scroll.set_child(Some(&icu_clamp));
    icu_page_box.append(&icu_scroll);

    let icu_nav_page = adw::NavigationPage::builder()
        .title("Intervals.icu")
        .tag("intervals")
        .child(&icu_page_box)
        .build();

    // ── Step 3: AI provider ───────────────────────────────────────────────

    let ai_page_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();

    let ai_header = adw::HeaderBar::new();
    ai_page_box.append(&ai_header);

    let ai_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let ai_clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    let ai_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    let ai_desc = gtk::Label::builder()
        .label(
            "Add an AI provider API key to get personalised coaching suggestions and \
             morning briefings. Supports Claude (Anthropic), OpenAI, and compatible APIs. \
             Your key is stored only on this device. This step is optional.",
        )
        .halign(gtk::Align::Start)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    ai_content.append(&ai_desc);

    let ai_group = adw::PreferencesGroup::builder()
        .title("Your AI Provider")
        .build();

    let ai_key_row = adw::PasswordEntryRow::builder().title("API Key").build();

    // Pre-populate
    if let Ok(Some(v)) = keystore::get_secret(keystore::KEY_ANTHROPIC) {
        ai_key_row.set_text(&v);
    }

    ai_group.add(&ai_key_row);
    ai_content.append(&ai_group);

    let ai_btn_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build();

    let skip_ai_btn = gtk::Button::builder()
        .label("Skip")
        .css_classes(["pill"])
        .tooltip_text("Skip AI setup for now")
        .build();
    let finish_btn = gtk::Button::builder()
        .label("Get Started")
        .css_classes(["pill", "suggested-action"])
        .tooltip_text("Finish setup and start using Cycle")
        .build();
    ai_btn_row.append(&skip_ai_btn);
    ai_btn_row.append(&finish_btn);
    ai_content.append(&ai_btn_row);

    ai_clamp.set_child(Some(&ai_content));
    ai_scroll.set_child(Some(&ai_clamp));
    ai_page_box.append(&ai_scroll);

    let ai_nav_page = adw::NavigationPage::builder()
        .title("AI Provider")
        .tag("ai")
        .child(&ai_page_box)
        .build();

    // ── Wire pages into NavigationView ────────────────────────────────────

    nav_view.add(&profile_nav_page);
    // icu_nav_page and ai_nav_page are pushed on demand

    // Next from profile → push intervals page
    {
        let nav = nav_view.clone();
        let icu_page = icu_nav_page.clone();
        let pool_p = pool.clone();
        let rt_p = rt_handle.clone();
        next_profile_btn.connect_clicked(move |_| {
            let name_val = name_row.text().trim().to_string();
            let ftp = ftp_adj.value() as u32;
            let weight = weight_adj.value() as f32;
            let max_hr = hr_adj.value() as u32;
            let id = athlete_id.get();

            let updated = crate::data::athlete::AthleteProfile {
                id,
                name: if name_val.trim().is_empty() {
                    "Athlete".to_string()
                } else {
                    name_val
                },
                ftp_watts: ftp,
                weight_kg: weight,
                max_hr,
                resting_hr: 60,
            };

            let p = pool_p.clone();
            rt_p.spawn(async move {
                if let Err(e) = db::update_athlete(&p, &updated).await {
                    tracing::error!("onboarding update_athlete: {e}");
                }
            });

            nav.push(&icu_page);
        });
    }

    // Skip intervals → push AI page directly
    {
        let nav = nav_view.clone();
        let ai_page = ai_nav_page.clone();
        skip_icu_btn.connect_clicked(move |_| {
            nav.push(&ai_page);
        });
    }

    // Next from intervals → push AI page, save settings
    {
        let nav = nav_view.clone();
        let ai_page = ai_nav_page.clone();
        let pool_i = pool.clone();
        let rt_i = rt_handle.clone();
        next_icu_btn.connect_clicked(move |_| {
            let key = icu_key_row.text().trim().to_string();
            let id = icu_id_row.text().trim().to_string();
            if !key.is_empty() {
                if let Err(e) = keystore::set_secret(keystore::KEY_INTERVALS_API, &key) {
                    tracing::error!("save intervals.api_key failed: {e}");
                } else {
                    tracing::debug!("Intervals.icu API key saved (not logged)");
                }
            }
            let p = pool_i.clone();
            rt_i.spawn(async move {
                let _ = settings::set_intervals_athlete_id(&p, &id).await;
            });
            nav.push(&ai_page);
        });
    }

    // Build the finish handler — shared by both Skip AI and Finish buttons
    let finish = {
        let pool_f = pool.clone();
        let rt_f = rt_handle.clone();
        let dialog_f = dialog.clone();
        let on_complete_f = Rc::clone(&on_complete);
        Rc::new(move |api_key: String| {
            if !api_key.is_empty() {
                if let Err(e) = keystore::set_secret(keystore::KEY_ANTHROPIC, &api_key) {
                    tracing::error!("save anthropic.api_key failed: {e}");
                } else {
                    tracing::debug!("AI provider API key saved (not logged)");
                }
            }
            let p = pool_f.clone();
            let rt = rt_f.clone();
            rt.spawn(async move {
                let _ = settings::mark_first_use_complete(&p).await;
            });
            dialog_f.close();
            on_complete_f();
        })
    };

    let finish_skip = Rc::clone(&finish);
    skip_ai_btn.connect_clicked(move |_| {
        finish_skip(String::new());
    });

    finish_btn.connect_clicked(move |_| {
        let key = ai_key_row.text().trim().to_string();
        finish(key);
    });

    // Present
    dialog.present(parent);
}
