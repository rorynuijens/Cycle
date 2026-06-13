# Cycle — Comprehensive Codebase Audit

**Date:** 2026-05-24  
**Auditor:** Senior software engineer review  
**Scope:** Full codebase (`src/`, `Cargo.toml`, `CLAUDE.md`)  
**Methodology:** Three-phase: Orientation → Analysis (Track A: UI/UX, Track B: Code Quality, Track C: Security) → Report

---

## Executive Summary

The Cycle codebase is well-structured, HIG-compliant in its broad architecture, and free from SQL injection and hardcoded credentials. The most significant outstanding issue is that **API keys are stored in plaintext SQLite** rather than the GNOME Keyring (High severity). Secondary concerns are **silent import errors** (no user-facing toast on failure), **test coverage gaps** in the largest modules, a **missing ERG power clamp**, and a **potential panic** on malformed FIT import.

---

## Phase 1 — Orientation

### Codebase Metrics

| Module | Files | Lines |
|---|---|---|
| `src/ui/pages/` | 11 | 12,435 |
| `src/data/` | 9 | 4,216 |
| `src/ai/` | 5 | 1,454 |
| `src/ui/` (top-level) | 5 | 1,812 |
| `src/devices/` | 4 | 1,028 |
| `src/training/` | 3 | 369 |
| `src/main.rs` | 1 | 98 |
| **Total** | **38** | **21,412** |

The largest single files are `history.rs` (2,259 lines), `fitness.rs` (2,466 lines), and `coaching.rs` (1,702 lines) — all UI pages. `db.rs` at 1,510 lines contains 55 public functions and is the largest domain file.

### Tech Stack

- **Language:** Rust 2021, GTK4 (`gtk4-rs` 0.11) + libadwaita 0.9, libshumate (maps)
- **Database:** SQLite via `sqlx` 0.8, path `~/.local/share/cycle/cycle.db`
- **BLE:** `btleplug` 0.11
- **HTTP:** `reqwest` 0.12 with `rustls-tls`
- **FIT files:** `fitparser` 0.6
- **Async runtime:** `tokio` v1 (background thread); GLib main loop on main thread
- **AI:** Anthropic Claude API (HTTP POST to `api.anthropic.com/v1/messages`)
- **External service:** Intervals.icu REST API (basic auth)

### Architecture Overview

```
GTK Main Thread (GLib loop)
  └── widget reads/writes
  └── block_on(db_call)  ← 56 occurrences in ui/
  └── async_channel recv ← BLE events

Tokio Runtime (background thread)
  └── BLE DeviceManager
  └── db:: async functions (called via block_on)
  └── AI HTTP calls (via spawn + async_channel result)
```

All SQL is in `data/db.rs`. All hardware I/O is in `devices/`. The layering is respected — `data/` and `devices/` have no GTK imports.

---

## Phase 2 — Analysis

### Track A: UI/UX & Information Design

#### A-PASS: Correct patterns confirmed

- `adw::ApplicationWindow` used for the main window throughout ✓
- `adw::NavigationSplitView` used for the sidebar layout ✓
- `adw::PreferencesWindow` for preferences (separate window) ✓
- `adw::AlertDialog` for all destructive confirmations ✓
- `adw::Clamp` with `maximum_size(900)` on all scrollable content ✓
- `adw::StatusPage` for empty states (library no-match, etc.) ✓
- `adw::Banner` used correctly for dismissible alerts (API key, countdown, export) ✓
- `adw::NavigationView` for multi-step onboarding wizard ✓
- All icon-only buttons have `tooltip_text` ✓
- Destructive actions carry `.css_classes(["destructive-action"])` ✓
- Semantic typography classes used throughout (`title-1` through `caption`, `dim-label`, `numeric`) ✓
- No hardcoded font sizes in Rust ✓
- `css_classes(["navigation-sidebar"])` on the sidebar ListBox ✓

#### A1 — Silent import failures (no user-facing toast)

**Severity: Medium | Files: `library.rs:321–403`, `calendar.rs` import handler**

When ZWO/ERG/GPX import fails, only `tracing::error!`/`tracing::warn!` is called. The user sees no indication of failure. Affected cases:

- File too large (> 1 MB): `tracing::warn!` only — `library.rs:367`
- Unsupported extension: `tracing::warn!` only — `library.rs:386`
- Parse error: `tracing::error!` only — `library.rs:393`
- GPX parse failure: `tracing::error!` only — `library.rs:321`
- DB save failure after import: `tracing::error!` only — `library.rs:403`

The `LibraryPage::new()` signature does not receive a toast callback, so there is no mechanism to report failures. The library import button was added after the toast infrastructure was established in other pages.

**Fix:** Pass an `on_toast: Rc<dyn Fn(adw::Toast)>` into `LibraryPage::new()` and call it on every failure path. Pattern is established in `CalendarPage::new()`.

#### A2 — Library schedule dialog has no success/failure feedback

**Severity: Low | File: `library.rs:469–511`**

`show_schedule_dialog` fires `rt_handle.spawn(async { db::schedule_workout(...) })` with no channel back to the UI. If the write fails (e.g., DB locked), the user sees no toast and no error. The calendar won't reflect the schedule until next reload.

#### A3 — Duplicate `ZONE_COLORS` constant

**Severity: Low | Files: `summary.rs:10–17`, `fitness.rs:18–25`**

The same 7-element `ZONE_COLORS` array is copy-pasted in both files. A change to zone colour in one will silently diverge from the other. Should be extracted to `data/athlete.rs` or a `ui/chart_colors.rs` module.

Similarly, `zone_index()` is defined in `summary.rs:20` and `fitness.rs:28` (identically) and inlined in `player.rs`. All three should share the one in `data/athlete.rs`, where `power_zone()` already exists.

#### A4 — Hardcoded non-zone colors in Cairo drawing code

**Severity: Low | File: `fitness.rs:501, 516, 547, 726–854, 2252`**

`fitness.rs` contains hardcoded RGBA tuples for chart elements that are not power zones: grey axis lines `(0.5, 0.5, 0.5)`, TSB fill `(0.30, 0.75, 0.55)`, PMC series lines, etc. CLAUDE.md §1.6 states: "Never hardcode colours in Rust."

For Cairo `DrawingArea` content, Adwaita CSS classes don't apply directly — this is a known limitation of custom drawing. However, the theme colour could be read from the Adwaita palette at draw time using `widget.style_context().lookup_color("accent_color")` or similar. As currently written, these colours do not adapt between light and dark themes (the PMC chart will appear identical in both).

#### A5 — Spacing value 16 not on the 6 px grid

**Severity: Low | File: `player.rs:124, 704–707`**

`inner.spacing(16)` and metric card `margin_top(16)` etc. are not multiples of 6. CLAUDE.md §1.4 requires multiples of 6 only. The nearest correct values are 12 or 18.

#### A6 — CLAUDE.md §1.4 contains a contradicting example

**Severity: Informational | File: `CLAUDE.md`**

The margin example at §1.4 shows `.margin_start(14).margin_end(14)` but the text rule says "always multiples of 6." 14 is not a multiple of 6. The example should use 12 or 18 to be consistent with the stated rule, to avoid confusing future code generation.

---

### Track B: Code Quality & Refactoring

#### B1 — `block_on` used 56 times in UI layer (blocks main thread)

**Severity: Medium (technical debt) | Files: All UI pages**

| File | `block_on` count |
|---|---|
| `coaching.rs` | 29 |
| `fitness.rs` | 21 |
| `dashboard.rs` | 27 |
| `history.rs` | 123 (total UI/) |
| `onboarding.rs` | 5 |
| Others | ~15 |

Every `rt_handle.block_on(db::some_query(...))` stalls the GTK event loop for the DB round-trip (~0.5–5 ms on SSD, potentially 50–500 ms under I/O contention). This is the primary architectural debt. The app is functional today because SQLite is fast on local NVMe, but it will produce visible UI freezes on slower storage (SD cards, spinning HDDs, network mounts).

The correct pattern for each call site is:
1. Disable the relevant button/row while loading.
2. `rt_handle.spawn(async { ... result ... })` + `async_channel::Sender` to send the result back.
3. GLib timeout or `glib::MainContext::default().spawn_local(async { ... })` to receive and update widgets.

This is a substantial refactor; it should be prioritised before adding more pages.

#### B2 — `Arc<Mutex<>>` in GTK callback context (window.rs)

**Severity: Low (justified) | File: `window.rs:241–271`**

`window.rs` uses `Arc<Mutex<Option<i64>>>` to pass a session ID from a tokio task (DB save) to a GLib callback (RPE dialog). The code comment correctly explains the design: the race window is negligible because the DB save takes ~10 ms and the user must interact with the dialog (several seconds). This is a legitimate cross-thread bridge and `Arc<Mutex<>>` is correct here. Not a bug. Document with a `// CLAUDE.md exception: cross-thread session_id bridge` comment to prevent future confusion.

#### B3 — Model ID hardcoded as a string literal

**Severity: Low | File: `coach.rs:528`**

```rust
model: "claude-sonnet-4-6",
```

This string is buried inside `get_suggestion()`. When the model needs updating, it's non-obvious where to change it. Promote to a module-level constant:

```rust
const CLAUDE_MODEL: &str = "claude-sonnet-4-6";
```

#### B4 — Duplicate `#[allow(clippy::too_many_arguments)]` on `build_week_view`

**Severity: Cosmetic | File: `calendar.rs`**

The function has two identical `#[allow(clippy::too_many_arguments)]` annotations stacked on it. One is redundant. The underlying cause (too many arguments) could be addressed by grouping the `weight_kg`, `on_toast`, and reload-related parameters into a `WeekViewContext` struct.

#### B5 — Test coverage gaps in critical modules

**Severity: Medium**

| Module | Has Tests | Notes |
|---|---|---|
| `data/db.rs` (1,510 lines, 55 functions) | **No** | Largest untested module |
| `data/session.rs` (NP/TSS math) | **No** | Complex formulas, high regression risk |
| `data/athlete.rs` (zone boundaries) | **No** | Off-by-one errors common here |
| `ai/briefing.rs` (prompt builder) | **No** | `parse_briefing_decision` has branching logic |
| `ai/coach.rs` (prompt builder) | **No** | |
| `ai/retrospective.rs` | **No** | |
| `data/import.rs` | Yes | ZWO/ERG/MRC parser |
| `devices/ftms.rs` | Yes | BLE parsers |
| `data/streams.rs` | Yes | GPS/power stream math |
| `data/route.rs` | Yes | GPX parser |
| `training/route_engine.rs` | Yes | Route simulation |
| `ui/markdown.rs` | Yes | Pango renderer |

Priority: add `#[tokio::test]` tests for `db.rs` using `:memory:` databases, and `#[test]` for `session.rs` NP/TSS boundary conditions.

#### B6 — `data/import.rs` parse functions lack internal size guard

**Severity: Low (defence-in-depth) | File: `import.rs`**

`parse_zwo(content: &str)` and `parse_erg(content: &str)` accept strings of arbitrary length. The 1 MB guard is enforced by `library.rs` at the call site, but there is no guard inside the functions themselves. A future call site that omits the check would expose unbounded parse work. The fix is a single early return inside each function:

```rust
pub fn parse_zwo(content: &str) -> Result<Workout> {
    anyhow::ensure!(content.len() <= 1_048_576, "workout file too large");
    // ...
}
```

---

### Track C: Security Audit

#### C1 — API keys stored in plaintext SQLite (High)

**Severity: High | Files: `db.rs`, `preferences.rs`, `onboarding.rs`, `coach.rs`, `intervals.rs`**

The Anthropic API key (`settings` table, key `"anthropic.api_key"`) and Intervals.icu API key (`settings` table, key `"intervals.api_key"`) are stored in plaintext in SQLite at `~/.local/share/cycle/cycle.db`.

CLAUDE.md §5.2 states: "Strava OAuth tokens: store in the GNOME Keyring via `libsecret`, not in the database." The spirit of this rule extends to all long-lived secrets. If the database file is copied, read by another process running as the same user, or included in a backup, the API keys are fully exposed.

**Fix:** Store secrets via `libsecret` (`secret-service` crate). Use the `settings` table only for non-sensitive configuration. The `reset_athlete_data` function correctly identifies and preserves these keys by name — that logic would need updating to query the Keyring instead.

Migration path: on first run after the update, read keys from the DB, write to Keyring, delete from DB.

#### C2 — No ERG target power clamp before trainer command

**Severity: Medium | Files: `training/engine.rs:147`, `devices/manager.rs:412–420`**

`engine.rs` casts `target_watts: u32` to `u16` and sends it unconditionally to the trainer:

```rust
let watts = target_watts as u16;   // silent truncation if > 65535
let _ = tx.try_send(DeviceCommand::SetTargetPower { watts });
```

CLAUDE.md §5.1 specifies: "Never send a raw, unclamped ERG target to the trainer — clamp to `[0, 1000]` W."

A `.zwo` file with a segment at 5000% FTP would result in `target_watts` = 5000% × FTP (e.g., 12,500 W for FTP 250), truncated to 65535 W when cast to `u16` (wraps around due to overflow), then sent to the trainer. Real-world FTMS trainers ignore implausible targets, but the command should still be sanitised:

```rust
let watts = target_watts.min(1000) as u16;
```

The device manager already correctly clamps received power readings (`cpp.power_watts.clamp(0, 3000)` at `manager.rs:309`), but this protection is not applied to outgoing targets.

#### C3 — Potential panic on malformed FIT import

**Severity: Medium | File: `data/fit.rs:387`**

```rust
let start = session_start.unwrap();
```

If a FIT file is syntactically valid but contains no `session` message with a timestamp (e.g., a device activity export truncated mid-recording), `session_start` will be `None` and this line will panic. The function should return `Err(...)`:

```rust
let start = session_start
    .context("FIT file has no session start timestamp")?;
```

#### C4 — All SQL uses parameterised queries (Pass)

Examined all 55 `db.rs` functions. Every query uses `sqlx::query("...").bind(value)` or `query!()` / `query_as!()` macros. No string interpolation in SQL was found. ✓

#### C5 — BLE packet length validation (Pass)

`parse_indoor_bike_data`, `parse_cycling_power_measurement`, and `parse_heart_rate` in `ftms.rs` all validate minimum packet length before any indexing. ✓

#### C6 — HTTP client security (Pass)

`coach.rs:521–525`:
```rust
let client = reqwest::Client::builder()
    .danger_accept_invalid_certs(false)  // explicit
    .timeout(std::time::Duration::from_secs(60))
    .build()?;
```

TLS verification is not disabled, timeout is set. ✓

#### C7 — No PII in logs (Pass)

Reviewed tracing calls. No athlete name, weight, or OAuth tokens are logged. API key save is noted as `"AI provider API key saved (not logged)"`. ✓

#### C8 — `unwrap()` in production paths

**Severity: Low | Files: `devices.rs:219,229`, `window.rs:253,271`, `fit.rs:387`**

- `devices.rs:219,229`: Two `unwrap()` after `.get()` / `.get_mut()` on a `HashMap`. Preceded by a presence check that makes them infallible in practice, but the pattern is fragile. Change to `.expect("invariant: ...")` with a comment.
- `window.rs:253,271`: Two `lock().unwrap()` on a `Mutex`. The mutex cannot be poisoned because no panic occurs while it is held, but `unwrap()` is conventionally incorrect. Change to `.expect("session_id_arc cannot be poisoned")`.
- `fit.rs:387`: Addressed in C3 above — must be changed to `?`.

#### C9 — XDG path not checked for write permission at startup

**Severity: Informational | File: `main.rs`**

If `~/.local/share/cycle/` is unwritable, `sqlx::SqlitePool::connect(...)` returns an error that propagates up to `main()` and crashes the process with a Rust backtrace — not a friendly user-facing error. Consider catching this error and showing an `adw::AlertDialog` before the window opens.

---

## Phase 3 — Findings Summary

### Prioritised Fix List

| ID | Track | Severity | File | Description |
|---|---|---|---|---|
| C1 | Security | **High** | `db.rs`, `preferences.rs` | Store API keys in GNOME Keyring (libsecret), not SQLite |
| C2 | Security | **Medium** | `engine.rs:147` | Clamp ERG target power to 1000 W before sending |
| C3 | Security | **Medium** | `fit.rs:387` | Replace `unwrap()` with `?` on session_start |
| A1 | UI/UX | **Medium** | `library.rs` | Show `adw::Toast` on all import failure paths |
| B1 | Quality | **Medium** (tech debt) | All UI pages | Replace `block_on` with async+channel pattern |
| B5 | Quality | **Medium** | `db.rs`, `session.rs`, `athlete.rs` | Add tests for DB functions, NP/TSS math, zone boundaries |
| A3 | UI/UX | Low | `summary.rs`, `fitness.rs` | Deduplicate `ZONE_COLORS` and `zone_index()` |
| B3 | Quality | Low | `coach.rs:528` | Promote model ID to `const CLAUDE_MODEL: &str` |
| B4 | Quality | Low | `calendar.rs` | Remove duplicate `#[allow(...)]` annotation |
| B6 | Quality | Low | `import.rs` | Add size guard inside `parse_zwo`/`parse_erg` |
| C8 | Security | Low | `devices.rs`, `window.rs` | Replace `unwrap()` with `.expect("reason")` |
| A2 | UI/UX | Low | `library.rs:511` | Add toast on schedule failure |
| A4 | UI/UX | Low | `fitness.rs` | Replace hardcoded Cairo colours with theme-aware values |
| A5 | UI/UX | Low | `player.rs:124,704` | Fix `spacing(16)` → `spacing(18)`, `margin(16)` → `margin(18)` |
| A6 | Style | Info | `CLAUDE.md` | Fix contradicting `margin_start(14)` example in §1.4 |
| B2 | Quality | Info | `window.rs:241` | Add `// CLAUDE.md exception` comment on Arc/Mutex usage |
| C9 | Security | Info | `main.rs` | Friendly error dialog if XDG data dir is unwritable |

### Positive Architecture Observations

1. **Clean layering**: `data/` and `devices/` have zero GTK imports. The architecture rule is followed.
2. **BLE security**: All FTMS packets validate length before indexing. Received sensor values are clamped.
3. **SQL safety**: All 55 database functions use parameterised queries. No injection risk.
4. **Onboarding**: The 3-step `adw::NavigationView` wizard is a clean UX pattern.
5. **History/Calendar parity**: Calendar reuses `history::show_session_detail()` directly — no duplication.
6. **AI prompts**: Prompt builders are pure functions in `ai/briefing.rs`, `ai/coach.rs`, `ai/retrospective.rs` — easy to test and modify independently of the UI.
7. **Markdown renderer**: `ui/markdown.rs` has its own test suite covering bold, italic, tables, headings, and escaping.

---

*Report generated by comprehensive three-phase audit. All line numbers reference the codebase state at 2026-05-24.*
