//! The guided program builder: a few questions, each a choice from a list.
//!
//! Nothing here is free text. Every answer is one of the options the rider is
//! shown, so the coach is told the same fixed words for the same answers (see
//! [`crate::data::training_profile`]). Each step opens on the rider's saved
//! answer, or on the recommended one, so a rebuild is a run of Next clicks.

use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::data::training_profile::{
    Approach, BackToBack, Experience, Goal, Length, Recovery, TrainingProfile,
};
use crate::ui::widgets::day_toggles::DayToggles;

use super::program::week_start;

/// Weeks ahead the event calendar opens on when no date is saved — a typical
/// build towards a target event.
const DEFAULT_EVENT_WEEKS: i64 = 12;

/// An option's title and the one line saying when to pick it.
type Choice = (&'static str, &'static str);

fn goal_choice(goal: Goal) -> Choice {
    match goal {
        Goal::ClimbingEvent => (
            "Climbing event or hilly gran fondo",
            "Long climbs decide the day",
        ),
        Goal::LongEvent => (
            "Long flat or rolling event",
            "A sportive or gran fondo where endurance matters most",
        ),
        Goal::Racing => ("Racing", "Road races or criteriums with surges and sprints"),
        Goal::TimeTrial => (
            "Time trial or triathlon",
            "Steady, hard power for the whole distance",
        ),
        Goal::GetFaster => ("No event — get faster", "Raise your FTP"),
        Goal::Fitness => (
            "No event — general fitness",
            "Stay fit and healthy, without chasing numbers",
        ),
    }
}

fn length_choice(length: Length) -> Choice {
    match length {
        Length::FourWeeks => ("4 weeks", "A short block to try a plan"),
        Length::EightWeeks => ("8 weeks", "Two full build cycles"),
        Length::TwelveWeeks => ("12 weeks", "Three build cycles, for a bigger change"),
        Length::Rolling => (
            "Keep rolling",
            "8 weeks at a time, continued when they run out",
        ),
    }
}

fn approach_choice(approach: Approach) -> Choice {
    match approach {
        Approach::CoachChooses => (
            "Let the coach choose",
            "Recommended — picked to suit your goal",
        ),
        Approach::Polarised => (
            "Polarised",
            "Mostly easy, some very hard, almost nothing in between",
        ),
        Approach::Pyramidal => (
            "Pyramidal",
            "Mostly easy, some tempo and sweet spot, a little very hard",
        ),
        Approach::SweetSpot => (
            "Sweet spot and threshold",
            "For short weeks — most rides are hard but controlled",
        ),
    }
}

fn recovery_choice(recovery: Recovery) -> Choice {
    match recovery {
        Recovery::NextDay => ("Fresh again", "Ready for another session"),
        Recovery::OneEasyDay => ("I need one easy day", "Then I am ready again"),
        Recovery::TwoPlusEasyDays => ("I need two or more easy days", "Hard days take a while"),
        Recovery::NotSure => ("Not sure", "The coach will decide"),
    }
}

fn back_to_back_choice(b: BackToBack) -> Choice {
    match b {
        BackToBack::Fine => ("Fine", "Two hard days in a row suit me"),
        BackToBack::Avoid => ("Avoid them", "Always an easy day between hard ones"),
        BackToBack::NotSure => ("Not sure", "The coach will decide"),
    }
}

fn experience_choice(experience: Experience) -> Choice {
    match experience {
        Experience::New => ("Under a year", "New to intervals and structured plans"),
        Experience::OneToThree => ("One to three years", "Used to following a plan"),
        Experience::ThreePlus => ("Over three years", "Experienced with structured training"),
    }
}

/// The short names of the training days: "Mon, Wed, Fri".
fn days_text(days: &[Weekday]) -> String {
    days.iter()
        .map(|d| match d {
            Weekday::Mon => "Mon",
            Weekday::Tue => "Tue",
            Weekday::Wed => "Wed",
            Weekday::Thu => "Thu",
            Weekday::Fri => "Fri",
            Weekday::Sat => "Sat",
            Weekday::Sun => "Sun",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One line describing the profile, for the Program section's summary row.
pub fn summary_line(profile: &TrainingProfile) -> String {
    let mut parts = vec![goal_choice(profile.goal).0.to_string()];
    match (profile.goal.has_event(), profile.event_date) {
        (true, Some(date)) => parts.push(date.format("%-d %b %Y").to_string()),
        (true, None) => {}
        (false, _) => parts.push(length_choice(profile.length).0.to_string()),
    }
    if profile.approach != Approach::CoachChooses {
        parts.push(approach_choice(profile.approach).0.to_string());
    }
    parts.push(days_text(&profile.training_days));
    parts.join(" · ")
}

/// What the event step says under the calendar for the chosen date.
fn event_status(profile: &TrainingProfile, start_monday: NaiveDate) -> Result<String, String> {
    match profile.event_week(start_monday) {
        Ok(week) => Ok(format!(
            "{week} weeks of training, tapering in weeks {} and {week}",
            week - 1
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// A list of options, one ticked, calling `on_pick` when the rider changes it.
fn choice_group<T: Copy + PartialEq + 'static>(
    title: &str,
    options: &[T],
    describe: fn(T) -> Choice,
    selected: T,
    on_pick: impl Fn(T) + 'static,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let on_pick = Rc::new(on_pick);
    let mut leader: Option<gtk::CheckButton> = None;
    for &option in options {
        let (label, subtitle) = describe(option);
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(option == selected)
            .build();
        if let Some(first) = &leader {
            check.set_group(Some(first));
        } else {
            leader = Some(check.clone());
        }
        let on_pick = Rc::clone(&on_pick);
        check.connect_toggled(move |c| {
            if c.is_active() {
                on_pick(option);
            }
        });
        let row = adw::ActionRow::builder()
            .title(label)
            .subtitle(subtitle)
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        group.add(&row);
    }
    group
}

/// A Next or Build pill for the bottom of a step.
fn step_button(label: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .build()
}

/// One step of the wizard: a header bar, then the content scrolled and clamped.
fn step_page(title: &str, tag: &str, intro: &str, content: &[&gtk::Widget]) -> adw::NavigationPage {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    body.append(
        &gtk::Label::builder()
            .label(intro)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build(),
    );
    for widget in content {
        body.append(*widget);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .child(&body)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    let page_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    page_box.append(&adw::HeaderBar::new());
    page_box.append(&scroll);
    adw::NavigationPage::builder()
        .title(title)
        .tag(tag)
        .child(&page_box)
        .build()
}

fn to_naive(date: &glib::DateTime) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(date.year(), date.month() as u32, date.day_of_month() as u32)
}

fn to_glib(date: NaiveDate) -> Option<glib::DateTime> {
    glib::DateTime::from_local(
        date.year(),
        date.month() as i32,
        date.day() as i32,
        12,
        0,
        0.0,
    )
    .ok()
}

/// Show the guided builder. `on_finish` runs with the rider's answers when they
/// press Build Program; closing the dialog any other way changes nothing.
pub fn show(
    parent: &impl IsA<gtk::Widget>,
    initial: TrainingProfile,
    on_finish: impl Fn(TrainingProfile) + 'static,
) {
    let start_monday = week_start(Local::now().date_naive());
    let draft = Rc::new(RefCell::new(initial));
    let nav_view = adw::NavigationView::new();

    // An explicit height for the same reason as the first-run wizard: every
    // step scrolls, and a ScrolledWindow reports its minimum as its natural
    // height, so a dialog left to size itself collapses to a sliver.
    let dialog = adw::Dialog::builder()
        .title("Build a Program")
        .content_width(480)
        .content_height(640)
        .build();
    dialog.set_child(Some(&nav_view));

    let snapshot = draft.borrow().clone();

    // ── Goal ──────────────────────────────────────────────────────────────
    let goal_group = choice_group(
        "What are you training for?",
        &Goal::ALL,
        goal_choice,
        snapshot.goal,
        {
            let draft = Rc::clone(&draft);
            move |goal| draft.borrow_mut().goal = goal
        },
    );
    let goal_next = step_button("Next", "Continue");
    nav_view.add(&step_page(
        "Your Goal",
        "goal",
        "A few questions so the coach can build a program around you. Each one has a \
         recommended answer already picked.",
        &[goal_group.upcast_ref(), goal_next.upcast_ref()],
    ));

    // ── Event date ────────────────────────────────────────────────────────
    let calendar = gtk::Calendar::new();
    let opening = snapshot
        .event_date
        .unwrap_or(start_monday + Duration::weeks(DEFAULT_EVENT_WEEKS) + Duration::days(5));
    if let Some(date) = to_glib(opening) {
        calendar.select_day(&date);
    }
    draft.borrow_mut().event_date = Some(opening);
    let calendar_card = gtk::Box::builder()
        .css_classes(["card"])
        .halign(gtk::Align::Center)
        .build();
    calendar_card.append(&calendar);
    let event_label = gtk::Label::builder()
        .halign(gtk::Align::Center)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let event_next = step_button("Next", "Continue");
    let refresh_event = {
        let draft = Rc::clone(&draft);
        let event_label = event_label.clone();
        let event_next = event_next.clone();
        move || {
            let status = event_status(&draft.borrow(), start_monday);
            match status {
                Ok(text) => {
                    event_label.set_label(&text);
                    event_label.set_css_classes(&["dim-label"]);
                    event_next.set_sensitive(true);
                }
                Err(text) => {
                    event_label.set_label(&text);
                    event_label.set_css_classes(&["error"]);
                    event_next.set_sensitive(false);
                }
            }
        }
    };
    calendar.connect_day_selected({
        let draft = Rc::clone(&draft);
        let refresh_event = refresh_event.clone();
        move |cal| {
            draft.borrow_mut().event_date = to_naive(&cal.date());
            refresh_event();
        }
    });
    nav_view.add(&step_page(
        "Event Date",
        "event",
        "When is your event? The program builds up to it and tapers in the last two weeks.",
        &[
            calendar_card.upcast_ref(),
            event_label.upcast_ref(),
            event_next.upcast_ref(),
        ],
    ));

    // ── Length (no event) ─────────────────────────────────────────────────
    let length_group = choice_group(
        "How long should this program run?",
        &Length::ALL,
        length_choice,
        snapshot.length,
        {
            let draft = Rc::clone(&draft);
            move |length| draft.borrow_mut().length = length
        },
    );
    let length_next = step_button("Next", "Continue");
    nav_view.add(&step_page(
        "Program Length",
        "length",
        "Without an event there is no finish line, so choose how far ahead to plan.",
        &[length_group.upcast_ref(), length_next.upcast_ref()],
    ));

    // ── Approach ──────────────────────────────────────────────────────────
    let approach_group = choice_group(
        "How do you want to train?",
        &Approach::ALL,
        approach_choice,
        snapshot.approach,
        {
            let draft = Rc::clone(&draft);
            move |approach| draft.borrow_mut().approach = approach
        },
    );
    let approach_next = step_button("Next", "Continue");
    nav_view.add(&step_page(
        "Approach",
        "approach",
        "How your week is split between easy and hard riding. If you have no preference, \
         leave it to the coach.",
        &[approach_group.upcast_ref(), approach_next.upcast_ref()],
    ));

    // ── Recovery ──────────────────────────────────────────────────────────
    let recovery_group = choice_group(
        "The day after a hard session, you feel…",
        &Recovery::ALL,
        recovery_choice,
        snapshot.recovery,
        {
            let draft = Rc::clone(&draft);
            move |recovery| draft.borrow_mut().recovery = recovery
        },
    );
    let back_to_back_group = choice_group(
        "Two hard days in a row?",
        &BackToBack::ALL,
        back_to_back_choice,
        snapshot.back_to_back,
        {
            let draft = Rc::clone(&draft);
            move |b| draft.borrow_mut().back_to_back = b
        },
    );
    let recovery_next = step_button("Next", "Continue");
    nav_view.add(&step_page(
        "Recovery",
        "recovery",
        "How you respond to training decides how hard days are spaced.",
        &[
            recovery_group.upcast_ref(),
            back_to_back_group.upcast_ref(),
            recovery_next.upcast_ref(),
        ],
    ));

    // ── Experience ────────────────────────────────────────────────────────
    let experience_group = choice_group(
        "How long have you trained with structure?",
        &Experience::ALL,
        experience_choice,
        snapshot.experience,
        {
            let draft = Rc::clone(&draft);
            move |experience| draft.borrow_mut().experience = experience
        },
    );
    let experience_next = step_button("Next", "Continue");
    nav_view.add(&step_page(
        "Experience",
        "experience",
        "Newer riders get a gentler build, with a recovery week every third week.",
        &[experience_group.upcast_ref(), experience_next.upcast_ref()],
    ));

    // ── Training days ─────────────────────────────────────────────────────
    let day_toggles = DayToggles::new(&snapshot.training_days);
    day_toggles.widget().set_halign(gtk::Align::Center);
    let days_next = step_button("Next", "Continue");
    days_next.set_sensitive(!snapshot.training_days.is_empty());
    day_toggles.connect_changed({
        let days_next = days_next.clone();
        move |count| days_next.set_sensitive(count > 0)
    });
    nav_view.add(&step_page(
        "Training Days",
        "days",
        "Which days can you train? Planned time off on your calendar is avoided \
         automatically.",
        &[day_toggles.widget().upcast_ref(), days_next.upcast_ref()],
    ));

    // ── Summary ───────────────────────────────────────────────────────────
    let summary_group = adw::PreferencesGroup::builder()
        .title("Your Answers")
        .build();
    let summary_rows: Vec<adw::ActionRow> = [
        "Goal",
        "When",
        "Approach",
        "After a hard day",
        "Hard days in a row",
        "Structured training",
        "Training days",
    ]
    .iter()
    .map(|title| {
        let row = adw::ActionRow::builder()
            .title(*title)
            .css_classes(["property"])
            .build();
        summary_group.add(&row);
        row
    })
    .collect();
    let build_btn = step_button("Build Program", "Ask the AI Coach to build this program");
    nav_view.add(&step_page(
        "Summary",
        "summary",
        "Check your answers. Go back to change any of them.",
        &[summary_group.upcast_ref(), build_btn.upcast_ref()],
    ));

    // ── Navigation ────────────────────────────────────────────────────────
    // The handlers below hold the navigation view, which the dialog owns, so
    // they capture it weakly (CLAUDE.md §2.4).
    goal_next.connect_clicked(glib::clone!(
        #[weak]
        nav_view,
        #[strong]
        draft,
        #[strong]
        refresh_event,
        move |_| {
            if draft.borrow().goal.has_event() {
                refresh_event();
                nav_view.push_by_tag("event");
            } else {
                nav_view.push_by_tag("length");
            }
        }
    ));
    for (button, next) in [
        (&event_next, "approach"),
        (&length_next, "approach"),
        (&approach_next, "recovery"),
        (&recovery_next, "experience"),
        (&experience_next, "days"),
    ] {
        button.connect_clicked(glib::clone!(
            #[weak]
            nav_view,
            move |_| nav_view.push_by_tag(next)
        ));
    }
    days_next.connect_clicked(glib::clone!(
        #[weak]
        nav_view,
        #[strong]
        draft,
        #[strong]
        day_toggles,
        move |_| {
            draft.borrow_mut().training_days = day_toggles.selected();
            let profile = draft.borrow().clone();
            let when = if profile.goal.has_event() {
                profile
                    .event_date
                    .map(|d| d.format("%A %-d %B %Y").to_string())
                    .unwrap_or_default()
            } else {
                length_choice(profile.length).0.to_string()
            };
            let values = [
                goal_choice(profile.goal).0.to_string(),
                when,
                approach_choice(profile.approach).0.to_string(),
                recovery_choice(profile.recovery).0.to_string(),
                back_to_back_choice(profile.back_to_back).0.to_string(),
                experience_choice(profile.experience).0.to_string(),
                days_text(&profile.training_days),
            ];
            for (row, value) in summary_rows.iter().zip(values) {
                row.set_subtitle(&value);
            }
            nav_view.push_by_tag("summary");
        }
    ));
    build_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            let mut profile = draft.borrow().clone();
            // A date left over from an earlier event answer would only confuse
            // whoever reads the stored profile next.
            if !profile.goal.has_event() {
                profile.event_date = None;
            }
            dialog.close();
            on_finish(profile);
        }
    ));

    nav_view.push_by_tag("goal");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("hardcoded valid date")
    }

    #[test]
    fn should_summarise_an_event_profile_in_one_line() {
        let profile = TrainingProfile {
            goal: Goal::ClimbingEvent,
            event_date: Some(date(2027, 3, 14)),
            approach: Approach::Polarised,
            training_days: vec![Weekday::Mon, Weekday::Wed, Weekday::Sat],
            ..TrainingProfile::default()
        };
        assert_eq!(
            summary_line(&profile),
            "Climbing event or hilly gran fondo · 14 Mar 2027 · Polarised · Mon, Wed, Sat"
        );
    }

    #[test]
    fn should_summarise_length_and_leave_out_a_coach_chosen_approach() {
        let profile = TrainingProfile {
            goal: Goal::GetFaster,
            event_date: Some(date(2027, 3, 14)),
            length: Length::Rolling,
            ..TrainingProfile::default()
        };
        assert_eq!(
            summary_line(&profile),
            "No event — get faster · Keep rolling · Mon, Wed, Fri"
        );
    }

    #[test]
    fn should_show_the_build_and_taper_weeks_for_a_valid_event() {
        // Monday 12 October 2026; Saturday 14 November is in week 5.
        let profile = TrainingProfile {
            goal: Goal::Racing,
            event_date: Some(date(2026, 11, 14)),
            ..TrainingProfile::default()
        };
        assert_eq!(
            event_status(&profile, date(2026, 10, 12)),
            Ok("5 weeks of training, tapering in weeks 4 and 5".to_string())
        );
    }

    #[test]
    fn should_refuse_an_event_too_soon_to_build_for() {
        let profile = TrainingProfile {
            goal: Goal::Racing,
            event_date: Some(date(2026, 10, 20)),
            ..TrainingProfile::default()
        };
        assert!(event_status(&profile, date(2026, 10, 12)).is_err());
    }

    #[test]
    fn should_open_the_calendar_on_a_date_the_wizard_accepts() {
        // The default event date must itself be valid, or the step would open
        // with Next already greyed out.
        let start = date(2026, 10, 12);
        let profile = TrainingProfile {
            goal: Goal::ClimbingEvent,
            event_date: Some(start + Duration::weeks(DEFAULT_EVENT_WEEKS) + Duration::days(5)),
            ..TrainingProfile::default()
        };
        assert_eq!(profile.event_week(start).unwrap(), 13);
    }
}
