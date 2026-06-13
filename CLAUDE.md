# CLAUDE.md — Cycle App Coding Standards

Guidelines for all code generation, review, and testing in this project.
This file is read by Claude at the start of every session.

---

## Project identity

- **App name:** Cycle
- **App ID:** `io.github.rorynuijens.Cycle`
- **Language:** Rust (2021 edition)
- **UI toolkit:** GTK4 + libadwaita (`gtk4-rs` / `libadwaita` crates)
- **Platform:** Linux (primary target: Fedora Silverblue / GNOME desktop)
- **Purpose:** Indoor cycling training — BLE smart trainer control, structured workouts, session recording

---

## 1. GNOME HIG compliance (non-negotiable)

Every UI decision must be justified against the [GNOME Human Interface Guidelines](https://developer.gnome.org/hig/).
Violations are bugs, not style preferences.

### 1.1 Widget hierarchy — always prefer Adwaita over plain GTK

| Situation | Use | Never use |
|---|---|---|
| Top-level window | `adw::ApplicationWindow` | `gtk::Window` |
| Two-pane layout | `adw::NavigationSplitView` | `gtk::Paned` |
| Page navigation | `adw::NavigationPage`, `adw::ViewStack` | manual widget swapping |
| Settings rows | `adw::ActionRow`, `adw::SwitchRow`, `adw::SpinRow`, `adw::ComboRow` | custom `gtk::Box` rows |
| Settings groups | `adw::PreferencesGroup` | plain `gtk::Frame` |
| Preferences window | `adw::PreferencesWindow` (separate window) | embedding prefs in main content |
| Notifications | `adw::Toast` via `adw::ToastOverlay` | `gtk::InfoBar` |
| Status / empty state | `adw::StatusPage` | custom empty-state widgets |
| Banners / alerts | `adw::Banner` | custom coloured bars |
| Header bar | `adw::HeaderBar` | `gtk::HeaderBar` |
| Clamp content width | `adw::Clamp` (max ~900 px) | unclamped full-width content |
| Loading spinner | `gtk::Spinner` | custom animations |
| Search | `gtk::SearchEntry` in header bar | inline search boxes |

### 1.2 Header bar rules

```rust
// CORRECT — title widget centred, actions at ends
let header = adw::HeaderBar::new();
header.pack_start(&back_button);
header.pack_end(&menu_button);
// Window title is set on AdwNavigationPage, not the header bar directly

// WRONG — never put a title label manually unless using a custom title widget
header.set_title_widget(Some(&gtk::Label::builder().label("My Page").build())); // avoid
```

- Window controls (close/min/max) are added by `adw::HeaderBar` automatically — never fake them
- The primary action button (e.g. "Start Workout") lives in `pack_start` of the content-side header bar
- Destructive actions (e.g. "End Workout") use `.css_classes(["destructive-action"])`
- Suggested actions use `.css_classes(["suggested-action"])`
- Never put more than one suggested-action button in a header bar

### 1.3 Navigation patterns

```rust
// Sidebar navigation: use AdwNavigationSplitView
let split = adw::NavigationSplitView::builder()
    .sidebar(&sidebar_nav_page)   // AdwNavigationPage
    .content(&content_nav_page)   // AdwNavigationPage
    .sidebar_width_fraction(0.22)
    .min_sidebar_width(200.0)
    .max_sidebar_width(280.0)
    .build();
// split.collapsed(true) automatically on narrow windows — do not suppress this

// Sidebar list: always use css_classes(["navigation-sidebar"])
let list = gtk::ListBox::builder()
    .css_classes(["navigation-sidebar"])
    .selection_mode(gtk::SelectionMode::Single)
    .build();
```

### 1.4 Spacing and margins

Follow the 6 px grid. Standard values:

```rust
// Page content margins
.margin_top(24).margin_bottom(24).margin_start(24).margin_end(24)

// Between cards / groups
.spacing(18)  // major sections
.spacing(12)  // items within a section
.spacing(6)   // tight rows

// Inside a card
.margin_top(12).margin_bottom(12).margin_start(12).margin_end(12)
```

Never use odd numbers (3, 7, 11, 13…) for spacing — always multiples of 6.

### 1.5 Typography classes

Use Adwaita semantic CSS classes — never hardcode font sizes:

```rust
// Hierarchy (largest to smallest)
"display"         // hero numbers (e.g. live power)
"title-1"         // page title
"title-2"         // section title
"title-3"         // card title
"title-4"         // subsection label
"heading"         // bold label
"body"            // default (no class needed)
"caption-heading" // small bold label
"caption"         // small label
"dim-label"       // secondary / de-emphasised text
"numeric"         // monospaced numbers (power, HR, cadence)
```

### 1.6 Colour and theming

- Never hardcode colours in Rust. All colour comes from:
  - Adwaita CSS classes (`"accent"`, `"success"`, `"warning"`, `"error"`)
  - Cairo drawing using `athlete.power_zone(watts).rgb()` — zone colours only
- Always support both light and dark themes (Adwaita handles this automatically if you don't hardcode)
- Test UI in both themes before marking a task complete

### 1.7 Accessibility

```rust
// Every interactive widget must have a tooltip or accessible label
button.set_tooltip_text("Start the selected workout");

// Icon-only buttons must have tooltip_text — the tooltip IS the label for screen readers
gtk::Button::builder()
    .icon_name("media-playback-start-symbolic")
    .tooltip_text("Start workout")   // required
    .build();

// Images that convey meaning need accessible descriptions
gtk::Image::builder()
    .icon_name("emblem-ok-symbolic")
    .build();
// If this communicates "connected", also set:
// widget.update_property(&[gtk::accessible::Property::Label("Connected")]);
```

---

## 2. Rust code style

### 2.1 Formatting and linting

```toml
# Always run before committing:
# cargo fmt
# cargo clippy -- -D warnings
```

- `cargo fmt` is mandatory — no PR merges without it
- `cargo clippy -D warnings` must pass cleanly
- Line width: 100 characters (set in `rustfmt.toml`)

```toml
# rustfmt.toml
max_width = 100
tab_spaces = 4
edition = "2021"
```

### 2.2 Error handling

```rust
// In library / domain code: use thiserror for typed errors
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("No active session")]
    NoActiveSession,
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

// In application / async code: use anyhow for propagation
pub async fn load_athlete(pool: &SqlitePool) -> anyhow::Result<AthleteProfile> {
    // ...
}

// In GTK callbacks: log errors, never panic or unwrap
some_button.connect_clicked(|_| {
    if let Err(e) = do_something() {
        tracing::error!("Failed to do something: {e}");
        // Show AdwToast to the user
    }
});

// Never use .unwrap() in production code paths — only in tests and sample data constructors
// Never use .expect() without a comment explaining why the panic is actually impossible
let date = NaiveDate::from_ymd_opt(2026, 1, 1)
    .expect("hardcoded valid date"); // acceptable
```

### 2.3 GTK threading model

This is the most important architecture rule in the entire codebase.

```
┌──────────────────────────────────┐
│  GTK MAIN THREAD (GLib loop)     │  <- ALL widget reads/writes here
│  • Widget construction           │
│  • GLib timeout callbacks        │
│  • async_channel receive poll    │
└──────────────────────────────────┘
            ↑ async_channel (thread-safe)
┌──────────────────────────────────┐
│  TOKIO RUNTIME (background)      │  <- BLE, SQLite, network
│  • DeviceManager                 │
│  • Database queries              │
│  • File I/O                      │
└──────────────────────────────────┘
```

```rust
// CORRECT — receive DeviceEvents on the main thread via GLib timeout
glib::timeout_add_local(Duration::from_millis(100), clone!(@strong event_rx => move || {
    while let Ok(event) = event_rx.try_recv() {
        // safe to update widgets here
        handle_device_event(event);
    }
    glib::ControlFlow::Continue
}));

// WRONG — never touch a GTK widget from a tokio task
tokio::spawn(async move {
    my_label.set_label("done"); // COMPILE ERROR — not Send, and would be UB
});

// WRONG — never block the main thread with async work
let result = tokio::runtime::Runtime::new().unwrap().block_on(fetch_data()); // blocks UI
```

### 2.4 Rc<RefCell<>> patterns for GTK callbacks

```rust
// Shared mutable state in GTK closures uses Rc<RefCell<>>, not Arc<Mutex<>>
// Rc is single-threaded (GTK main thread), RefCell gives interior mutability

let engine = Rc::new(RefCell::new(WorkoutEngine::new(workout, athlete, cmd_tx)));

// In a GLib closure, clone the Rc (cheap) before moving into the closure
let engine_clone = Rc::clone(&engine);
glib::timeout_add_local(Duration::from_secs(1), move || {
    let mut eng = engine_clone.borrow_mut();
    let snapshot = eng.tick(get_readings());
    drop(eng); // release borrow before updating widgets
    update_ui(&snapshot);
    glib::ControlFlow::Continue
});

// Never hold a RefCell borrow across an await point or across widget updates
// that could re-enter the borrow (e.g. via signal emissions)
```

### 2.5 Naming conventions

```rust
// Types: PascalCase
pub struct WorkoutEngine { ... }
pub enum PowerZone { ... }

// Functions and methods: snake_case
pub fn start_workout(&mut self) { ... }
pub fn format_duration(secs: u32) -> String { ... }

// Constants: SCREAMING_SNAKE_CASE
pub const APP_ID: &str = "io.github.yourname.Cycle";
pub const FTMS_SERVICE_UUID: &str = "00001826-...";

// GTK widget local variables: descriptive snake_case, suffix with type where ambiguous
let start_btn = gtk::Button::builder()...;
let workout_progress = gtk::ProgressBar::builder()...;
let power_label = gtk::Label::builder()...;

// Page structs: suffix with Page
pub struct DashboardPage { ... }
pub struct PlayerPage { ... }

// Widget structs: descriptive, no suffix required
pub struct WorkoutGraph { ... }
```

### 2.6 Module structure

```
src/
├── main.rs          # Entry point only — no business logic
├── data/            # Pure domain types — zero GTK imports allowed
│   ├── mod.rs
│   ├── athlete.rs
│   ├── workout.rs
│   ├── session.rs
│   └── db.rs
├── devices/         # Hardware — zero GTK imports allowed
│   ├── mod.rs
│   ├── peripheral.rs
│   ├── ftms.rs
│   └── manager.rs
├── training/        # Business logic — zero GTK imports allowed
│   ├── mod.rs
│   └── engine.rs
└── ui/              # GTK4/Adwaita — only layer that imports gtk/adw
    ├── mod.rs
    ├── window.rs
    ├── pages/
    └── widgets/
```

**Rule:** `data/`, `devices/`, and `training/` modules must never import `gtk` or `adw`.
If you need to pass UI state into these modules, use plain Rust types — not GObjects.

### 2.7 Comments and documentation

```rust
// Public API: always document with ///, include what errors can occur
/// Returns the Coggan power zone for a given wattage.
///
/// Uses the athlete's current FTP as the reference.
/// Returns `PowerZone::Neuromuscular` for any power above 150% FTP.
pub fn power_zone(&self, watts: u32) -> PowerZone { ... }

// Implementation comments: explain WHY, not WHAT
// GTK must only be touched from the main thread — see CLAUDE.md §2.3
glib::timeout_add_local(...);

// TODO comments must include a tracking note
// TODO(ble): implement btleplug scan — see DeviceCommand::StartScan stub
```

---

## 3. Testing patterns

### 3.1 What to test and where

```
data/       → unit tests (pure functions, no GTK, no async)
devices/    → unit tests for parsers; integration tests for BLE (feature-flagged)
training/   → unit tests for engine logic
ui/         → manual testing + screenshot comparison (GTK cannot be headless easily)
```

### 3.2 Unit test structure

Place unit tests in the same file as the code under test:

```rust
// At the bottom of data/athlete.rs:
#[cfg(test)]
mod tests {
    use super::*;

    // Name tests: should_<behaviour>_when_<condition>
    #[test]
    fn should_return_z4_threshold_when_power_is_exactly_ftp() {
        let athlete = AthleteProfile {
            ftp_watts: 250,
            ..AthleteProfile::default()
        };
        assert_eq!(athlete.power_zone(250), PowerZone::Threshold);
    }

    #[test]
    fn should_return_z1_recovery_when_power_is_zero() {
        let athlete = AthleteProfile::default();
        assert_eq!(athlete.power_zone(0), PowerZone::ActiveRecovery);
    }

    // Test zone boundaries explicitly — off-by-one errors are common here
    #[test]
    fn should_return_z3_tempo_at_90_percent_ftp() {
        let athlete = AthleteProfile { ftp_watts: 200, ..AthleteProfile::default() };
        assert_eq!(athlete.power_zone(180), PowerZone::Tempo); // 90% of 200
    }

    #[test]
    fn should_return_z4_threshold_at_91_percent_ftp() {
        let athlete = AthleteProfile { ftp_watts: 200, ..AthleteProfile::default() };
        assert_eq!(athlete.power_zone(182), PowerZone::Threshold); // 91% of 200
    }
}
```

### 3.3 Testing the workout engine

```rust
// training/engine.rs tests:
#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{athlete::AthleteProfile, workout::Workout};

    fn make_engine() -> WorkoutEngine {
        // Use a mock channel — the engine sends commands but we discard them
        let (cmd_tx, _cmd_rx) = async_channel::bounded(16);
        WorkoutEngine::new(
            Workout::sample_threshold(),
            AthleteProfile { ftp_watts: 250, ..AthleteProfile::default() },
            cmd_tx,
        )
    }

    #[test]
    fn should_start_in_idle_state() {
        let engine = make_engine();
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[test]
    fn should_advance_to_running_on_start() {
        let mut engine = make_engine();
        engine.start();
        assert_eq!(engine.state, EngineState::Running);
    }

    #[test]
    fn should_return_correct_segment_at_elapsed_time() {
        let engine = make_engine();
        // Sample workout: warmup 600s, then interval 480s
        let (seg_idx, seg_elapsed) = engine.segment_at(601);
        assert_eq!(seg_idx, 1);       // second segment (interval 1)
        assert_eq!(seg_elapsed, 1);   // 1 second into it
    }

    #[test]
    fn should_format_duration_correctly() {
        assert_eq!(WorkoutEngine::format_duration(0),    "0:00");
        assert_eq!(WorkoutEngine::format_duration(60),   "1:00");
        assert_eq!(WorkoutEngine::format_duration(3599), "59:59");
        assert_eq!(WorkoutEngine::format_duration(3600), "60:00");
    }

    #[test]
    fn normalised_power_requires_30_seconds_of_data() {
        let mut session = crate::data::session::Session::new(None);
        // Only 10 data points — NP should be None
        for i in 0..10 {
            session.data_points.push(crate::data::session::DataPoint {
                elapsed_secs: i,
                power_watts: Some(200),
                heart_rate_bpm: None,
                cadence_rpm: None,
                speed_kmh: None,
            });
        }
        assert!(session.normalised_power().is_none());
    }
}
```

### 3.4 Testing the FTMS parser

```rust
// devices/ftms.rs tests:
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_power_from_known_ftms_packet() {
        // Flags: bit 6 set (power present), bit 0 clear (speed present)
        // Speed = 0x0BB8 = 3000 → 30.00 km/h
        // Power = 0x0118 = 280 W
        let data = &[0x44, 0x00, 0xB8, 0x0B, 0x18, 0x01];
        let result = parse_indoor_bike_data(data).unwrap();
        assert_eq!(result.power_watts, Some(280));
        assert_eq!(result.speed_kmh, Some(30.0));
    }

    #[test]
    fn should_return_none_for_too_short_packet() {
        assert!(parse_indoor_bike_data(&[0x00]).is_none());
    }

    #[test]
    fn should_build_correct_set_target_power_command() {
        let cmd = set_target_power_command(300);
        assert_eq!(cmd, vec![0x05, 0x2C, 0x01]); // 300 = 0x012C, little-endian
    }

    #[test]
    fn should_build_request_control_command() {
        assert_eq!(request_control_command(), vec![0x00]);
    }
}
```

### 3.5 Async / database tests

```rust
// Use #[tokio::test] for async tests
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_create_database_and_tables() {
        // Use an in-memory SQLite database for tests — never touch the real XDG path
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        migrate(&pool).await.expect("migration should succeed");

        // Verify tables exist by inserting a row
        sqlx::query("INSERT INTO athletes (name, ftp_watts) VALUES ('Test', 250)")
            .execute(&pool)
            .await
            .expect("insert should succeed");
    }
}
```

### 3.6 Running tests

```bash
# All tests
cargo test

# A specific module
cargo test data::athlete

# A specific test by name
cargo test should_return_z4_threshold

# With output (useful for debugging)
cargo test -- --nocapture

# Check for test coverage (requires cargo-tarpaulin)
cargo tarpaulin --out Html
```

---

## 4. Code review checklist

Apply this checklist when reviewing any PR or generated code.

### 4.1 Architecture

- [ ] Does new code in `data/`, `devices/`, or `training/` import `gtk` or `adw`? → **Reject**
- [ ] Is GTK widget state being read or written from outside the main thread? → **Reject**
- [ ] Is any background work (BLE, SQLite, network) done directly in a GTK callback? → **Reject**
- [ ] Is `Rc<RefCell<>>` used for shared GTK state (not `Arc<Mutex<>>`)?
- [ ] Are `async_channel` used correctly to bridge tokio ↔ GLib?

### 4.2 GNOME HIG

- [ ] Is `adw::ApplicationWindow` used (not `gtk::Window`)?
- [ ] Is `adw::NavigationSplitView` used for the two-pane layout?
- [ ] Are preferences in a separate `adw::PreferencesWindow`?
- [ ] Do all icon-only buttons have `tooltip_text`?
- [ ] Are destructive actions styled with `"destructive-action"` CSS class?
- [ ] Is content width clamped with `adw::Clamp`?
- [ ] Are spacing values multiples of 6 px?
- [ ] Are Adwaita semantic typography classes used (no hardcoded font sizes)?
- [ ] Are colours coming from Adwaita classes or zone RGB values (not hardcoded hex)?
- [ ] Does the layout work at both 900 px and 1400 px window widths?
- [ ] Has the feature been tested in both light and dark themes?

### 4.3 Rust quality

- [ ] Does `cargo fmt` produce no changes?
- [ ] Does `cargo clippy -- -D warnings` pass cleanly?
- [ ] Are there any `.unwrap()` calls in non-test, non-sample-data code? → **Justify or remove**
- [ ] Are errors propagated correctly (thiserror for domain, anyhow for application)?
- [ ] Are GTK callback errors logged (not silently swallowed)?
- [ ] Is `RefCell::borrow_mut()` dropped before any widget update that could re-enter?
- [ ] Are public functions documented with `///`?

### 4.4 Tests

- [ ] Does new domain logic have unit tests?
- [ ] Are zone boundary conditions tested?
- [ ] Do FTMS parser tests cover both valid and malformed packets?
- [ ] Are async database operations tested against `:memory:` (not the real XDG path)?
- [ ] Does `cargo test` pass cleanly?

---

## 5. Security review checklist

### 5.1 BLE / hardware attack surface

```rust
// ALWAYS validate packet length before indexing
pub fn parse_indoor_bike_data(data: &[u8]) -> Option<IndoorBikeData> {
    if data.len() < 2 {
        return None; // never panic on short packets from untrusted hardware
    }
    // Bounds-check before every read
    if offset + 2 <= data.len() {
        let value = u16::from_le_bytes([data[offset], data[offset + 1]]);
        // ...
    }
}

// Clamp sensor values to physiologically plausible ranges
// A rogue BLE device could send 65535 W — display it, but don't use it for ERG commands
fn sanitise_power(raw: u32) -> u32 {
    raw.clamp(0, 3000) // 3000 W is well above any human capability
}

fn sanitise_cadence(raw: u32) -> u32 {
    raw.clamp(0, 250) // 250 rpm is the physical maximum
}

fn sanitise_hr(raw: u32) -> u32 {
    raw.clamp(0, 250) // 250 bpm is the medical maximum
}
```

- Never send a raw, unclamped ERG target to the trainer — clamp to `[0, 1000]` W
- Validate that a device claiming FTMS actually responds to control commands before trusting it
- Log unexpected packet structures at `tracing::warn!` level for forensics

### 5.2 Database

```rust
// ALWAYS use parameterised queries — never string interpolation in SQL
// CORRECT
sqlx::query("SELECT * FROM workouts WHERE id = ?")
    .bind(workout_id)
    .fetch_one(pool)
    .await?;

// WRONG — SQL injection risk even with internal data
let query = format!("SELECT * FROM workouts WHERE name = '{}'", name); // never
```

- The database lives at `~/.local/share/cycle/cycle.db` — always use the XDG path helper
- Never store credentials, API tokens, or personal health data in plaintext in SQLite
  - Strava OAuth tokens: store in the GNOME Keyring via `libsecret`, not in the database
- Validate all data read from SQLite before using it in the UI (DB could be corrupted or tampered)

### 5.3 File I/O and paths

```rust
// CORRECT — use XDG directories only
fn workout_library_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".local/share"));
    data_dir.join("cycle").join("workouts")
}

// When importing user-supplied .zwo / .erg files:
// 1. Validate the file extension before reading
// 2. Limit file size (e.g. reject files > 1 MB — no legitimate workout file is larger)
// 3. Parse defensively — treat all fields as untrusted
// 4. Never execute or eval any content from imported files
const MAX_WORKOUT_FILE_SIZE_BYTES: u64 = 1_048_576; // 1 MB

fn import_workout_file(path: &Path) -> anyhow::Result<Workout> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.len() <= MAX_WORKOUT_FILE_SIZE_BYTES,
        "Workout file too large ({} bytes)", meta.len()
    );
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    anyhow::ensure!(
        matches!(ext, "zwo" | "erg" | "mrc"),
        "Unsupported workout file format: {}", ext
    );
    // ... parse
}
```

### 5.4 Network / API

```rust
// Strava OAuth: always use PKCE flow — never embed client secrets in the binary
// Redirect URI must be a localhost URI (e.g. http://127.0.0.1:PORT/callback)
// Never log access tokens — use tracing::debug!("Token acquired (not logged)")

// Validate TLS — never disable certificate verification
let client = reqwest::Client::builder()
    .danger_accept_invalid_certs(false) // this is the default; be explicit
    .build()?;

// All network calls must have timeouts
let response = client
    .get("https://www.strava.com/api/v3/athlete")
    .timeout(Duration::from_secs(10))
    .send()
    .await?;
```

### 5.5 Dependency hygiene

```bash
# Run before any release:
cargo audit                      # check for known CVEs in dependencies
cargo deny check                 # verify license compliance and ban list
cargo update                     # keep dependencies current
```

```toml
# Cargo.deny — deny.toml
[advisories]
vulnerability = "deny"
unmaintained  = "warn"
yanked        = "deny"

[licenses]
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception"]
```

### 5.6 Logging — never log personal data

```rust
// CORRECT
tracing::info!("Athlete profile loaded (id={})", athlete.id);
tracing::debug!("BLE packet received ({} bytes)", data.len());

// WRONG — PII / health data in logs
tracing::debug!("Athlete: name={}, weight={}", athlete.name, athlete.weight_kg); // never
tracing::debug!("HR reading: {} bpm", hr); // fine — it's a measurement, not PII
tracing::debug!("OAuth token: {}", token); // never
```

---

## 6. Interaction with Claude

When asking Claude to write code for this project:

1. **Always specify the file path** — Claude writes to the exact module the code belongs in
2. **Mention thread context** — "this runs in a GTK callback" or "this runs in a tokio task" so Claude applies the correct patterns
3. **Reference this file** — "following CLAUDE.md" reminds Claude to apply these standards
4. **Ask for tests alongside implementation** — "implement X and write unit tests for it"
5. **Flag GTK version constraints** — this project targets GTK 4.12 / libadwaita 1.5; ask Claude to avoid APIs added after those versions

### Prompt patterns that work well

```
"Implement [feature] in src/[path].rs following CLAUDE.md.
 This code runs on the GTK main thread.
 Include unit tests for boundary conditions."

"Review the following code against the CLAUDE.md security checklist
 and HIG compliance rules. List each violation with a fix."

"Refactor [function] to handle errors using anyhow::Result
 instead of unwrap(), following CLAUDE.md §2.2."
```

---

## 7. Quick reference — common mistakes to avoid

| Mistake | Correct approach |
|---|---|
| `use gtk::prelude::*` alongside `use adw::prelude::*` | Use only `adw::prelude::*` — it re-exports all GTK traits |
| `stack.add_named(&widget, ...)` | `stack.add_named(widget, ...)` — no leading `&` |
| `widget_name().map(...)` | `widget_name().as_str()` — GString is not Option |
| `f32` arithmetic with Cairo (which uses `f64`) | Cast to `f64` before Cairo operations |
| `Arc<Mutex<>>` for GTK shared state | `Rc<RefCell<>>` — GTK is single-threaded |
| `tokio::main` without a separate GLib runtime | Run GLib loop on main thread, tokio in a separate thread |
| Hardcoded pixel sizes for fonts | Adwaita CSS classes (`"title-1"`, `"caption"`, etc.) |
| Hardcoded colours | Adwaita CSS classes or `PowerZone::rgb()` for zone colours |
| `gtk::Window` | `adw::ApplicationWindow` |
| `gtk::Paned` for sidebar layout | `adw::NavigationSplitView` |
| Embedding preferences in main window | `adw::PreferencesWindow` (separate window) |
| `.unwrap()` in GTK callbacks | Match on Result, log errors, show `adw::Toast` |
| Raw SQL string interpolation | `sqlx::query(...).bind(value)` |
| Storing OAuth tokens in SQLite | GNOME Keyring via `libsecret` |
