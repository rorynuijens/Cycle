# Refactor plan — reduce code size without losing functionality

**Repo:** `rorynuijens/Cycle` (Tour du Salon) · **Baseline:** release 0.23.1
**Goal:** remove 10–20 % of non-test Rust (≈ 7,000–14,000 lines) by removing duplication,
with zero change in behaviour. Shorten the tests where possible, but never reduce what they cover.

> **How to run this in Claude Code:** open the repo and say
> *"Follow docs/REFACTOR_PLAN.md. Do Phase 0, then stop and show me the report."*
> After that, take one phase per session: *"Do Phase N of docs/REFACTOR_PLAN.md."*
> Each phase stops for review before the next one starts.
> Phase 7 (Blueprint pilot) doesn't depend on Phases 1–6 and can run any time on its own
> branch: *"Do Phase 7 of docs/REFACTOR_PLAN.md."*

### Run order

The rider plans training in this app, so the calendar is core and daily-use. Its cleanup
comes early, and planning must work without AI or any outside service.

| Step | Phase | Why here |
|---|---|---|
| 1 | Phase 0 — Measure | baseline for everything else |
| 2 | Phase 1 — Shared player plumbing | lowest risk; proves the workflow |
| 3 | **Phase 3 — Calendar pages** | daily-use planning surface, ~5,000 lines, no UI tests |
| 4 | Phase 8 — Planning without AI | must hold before more planning code moves |
| 5 | Phase 9 — Garmin first, Intervals.icu optional | feature work; see the reason there |
| 6 | Phase 2 — Player constructors | |
| 7 | Phase 4 — Rest of the UI | |
| 8 | Phase 5 — Test compaction | |
| 9 | Phase 6 — Wrap-up | |
| any time | Phase 7 — Blueprint pilot | independent; its rollout puts the calendar second |

Phases 8 and 9 are **not** pure refactors. They change behaviour on purpose, so ground rule 2
(no behaviour changes) is replaced inside them by their own rules.

---

## Ground rules (apply to every phase)

1. **CLAUDE.md wins.** Every change must still meet CLAUDE.md: HIG widgets, the 6 px grid,
   no colours in Rust, no `gtk`/`adw` in `data/`, `devices/`, `training/`, weak captures in
   dialog handlers (§2.4), no new `.unwrap()`, and GTK 4.12 / libadwaita 1.5 APIs only.
2. **No behaviour changes.** No feature removed, no UI text changed, no layout changed, no
   change to the DB schema or on-disk formats, no change to BLE/ANT+ byte handling.
   If a dedup *would* change behaviour (even slightly), leave the code as it is and write it
   down in the phase report.
3. **Tests are a safety net, not padding.** Never delete a test case. Turning N near-identical
   tests into one table-driven test is allowed only if every input/expected pair is kept.
   The `cargo test` count of *cases* must not go down (use the case-count check in Phase 0).
4. **One refactor per commit.** Each commit is small, compiles, passes the gate, and has a
   message like `refactor(player): share connected-device list between players (-62 lines)`.
5. **Gate after every commit:**
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
   If the gate fails and the fix isn't obvious within two tries, revert that commit
   (`git reset --hard HEAD~1`), note it in the report, and move on.
6. **Work on a branch:** `git switch -c refactor/dedup-<phase>` from `main`. Never push to
   `main`. Push the branch and open a PR only when the rider asks.
7. **Don't game the count.** Don't join lines, strip doc comments, remove blank lines that
   `rustfmt` keeps, or move Rust into data files just to lower the number. The saving must come from
   code that no longer exists.
8. **Stop and ask** before: touching `devices/` byte parsers, `data/db/` migrations,
   `data/fit.rs` encoding, or anything in the 3D scene that affects GPX elevation or ride
   physics (see the `stylized-route` skill's hard rules).

---

## Phase 0 — Measure (no code changes)

Make a baseline that every later phase is measured against.

1. Install the tooling locally (it isn't needed in CI):
   ```bash
   cargo install tokei        # line counts
   npm i -g jscpd             # copy-paste detection
   ```
2. Record the baseline in `docs/refactor/baseline.md`:
   - `tokei src` (code / comments / blanks)
   - non-test lines vs test lines per file. Write `scripts/loc_split.py` to count as test
     code: (a) the brace-matched body of every `#[cfg(test)] mod … { … }` block, (b) every
     file named `tests.rs` or under a `tests/` directory, and (c) every file whose `mod`
     declaration is itself marked `#[cfg(test)]` (e.g. `ui/pages/calendar/screenshots.rs`).
     Print `non_test test file`, sorted by non-test lines. Don't count from the first
     `#[cfg(test)]` to the end of the file: some files put `#[cfg(test)]` on single items
     near the top (`data/course3d.rs` does, at line 56), and that undercounts their app
     code by thousands of lines.
   - number of test cases: `cargo test -- --list 2>/dev/null | grep -c ': test$'`
   - jscpd report:
     `jscpd src --format rust --min-lines 8 --min-tokens 60 --reporters json,console --output docs/refactor/jscpd`
3. From the jscpd output, list the **top 25 file pairs by duplicated lines**, and for each
   pair, list the specific clones (file:line ranges).
4. Write a **ranked refactor list** in `docs/refactor/targets.md`. For each target, give:
   the files, the estimated lines saved, the risk (low/med/high), and the phase it belongs to.
   Merge in the known hotspots below and confirm or reject each one with numbers.
5. **Stop.** Show the rider the baseline totals and the ranked list. Don't start Phase 1 until
   they approve it (they may reorder or drop targets).

### Known hotspots (measured at 0.23.1, verify in Phase 0)

| Area | Evidence | Likely saving |
|---|---|---|
| `ui/pages/player.rs` ↔ `ui/pages/route_player.rs` | Same-name methods ~95–100 % identical: `add_connected_device`, `remove_connected_device`, `set_stale_sensors`, `set_readings`, `widget`, `overlay_is_open`, `close_overlay`, `toggle_overlay`. `attach_overlay` 70 %, `add_intensity_shortcuts` 77 %. `new` (~500 lines each) ~45 % similar. `start_timer` only 17 % — **leave it alone**. | 300–700 |
| `ui/pages/calendar/{detail,dialogs,screenshots,week}.rs` | ~5,350 lines, all UI, no tests. 12 dialog/row builders each in `detail.rs` and `dialogs.rs`. | 500–1,500 |
| Large test modules | `training/program.rs` (1,563 test lines), `devices/ant.rs` (1,013), `training/ftp_detect.rs` (896), `ai/garmin/sync/tests.rs` (850), `data/db/sessions.rs` (824), `data/fit.rs` (685) | 1,500–4,000 (test lines, same cases) |
| Biggest UI files | `ui/widgets/course_view.rs` (2,486), `ui/pages/coaching/plan.rs` (1,675), `ui/pages/devices.rs` (1,351), `ui/widgets/ride_hud.rs` (1,272), `ui/pages/dashboard.rs` (1,219) | find in Phase 0 |

---

## Phase 1 — Shared player plumbing (low risk)

Target: the identical methods in the two players.

1. Make a new module `src/ui/pages/player_common.rs` (or `ui/widgets/ride_chrome.rs` if it
   reads better). Put in it a struct, e.g. `RideChrome`, that owns the pieces both players build the
   same way: connected-device list, stale-sensor banner, readings sink, overlay/toast handling,
   and intensity keyboard shortcuts.
2. Both `PlayerPage` and `RoutePlayerPage` hold a `RideChrome` and forward to it, or they expose
   it directly if callers in `ui/window/` can be updated simply.
3. `add_intensity_shortcuts` differs between the two (one takes two callbacks, the other takes
   a dial closure). Unify it on the dial form (`Fn(i32)`) and adapt the workout player's
   up/down callbacks to it.
4. `attach_overlay`: pull out the shared 65–76 lines and pass what differs in as a parameter.
5. Keep `start_timer` and `update_cue` separate in each player; they really are different.
6. **Manual check (ask the rider):** start a structured workout and a route ride, connect and
   disconnect a sensor, let a sensor go stale, open/close the overlay, and use the intensity
   shortcuts. Check in light and dark mode.

Report: lines saved, anything that was left duplicated and why.

## Phase 2 — Player constructors (medium risk)

Target: the ~45 %-similar `new()` functions (~500 lines each).

1. Diff the two `new()` bodies block by block. Pull out shared *builders* (header bar,
   HUD layout, metric columns, the breakpoint/compact layout, fullscreen handling) as free
   functions or `RideChrome` methods that return widgets.
2. Don't build a generic "player trait". Plain shared functions are enough; traits here tend
   to add lines rather than remove them.
3. Keep every weak capture weak (CLAUDE.md §2.4). Search the diff for any new strong clone
   of a window/dialog captured inside a child's handler.
4. Manual check as in Phase 1, plus: resize to 900 px and 1400 px, and check the compact
   breakpoint.

## Phase 3 — Calendar pages (medium risk)

Target: `ui/pages/calendar/` (~5,350 lines).

1. Use Phase 0's jscpd clones for these files to list repeated shapes: dialog scaffolding,
   `adw::ActionRow` groups, date/duration rows, confirm-delete alerts, screenshot grids.
2. Put shared builders in `ui/widgets/dialog.rs` (it already exists; extend it) or a new
   `ui/pages/calendar/rows.rs`.
3. Every destructive confirmation must keep `destructive-action` styling and its wording.
4. Manual check: open every calendar dialog, edit and delete a session, view screenshots,
   switch week/month.

## Phase 4 — Rest of the UI (medium risk)

Go down the Phase 0 ranked list for the other UI files (`course_view.rs`, `coaching/plan.rs`,
`devices.rs`, `ride_hud.rs`, `dashboard.rs`, `library/`, `preferences/`). Only take clones
of **≥ 15 lines**; smaller ones usually aren't worth the extra indirection.
For `course_view.rs`, follow the `stylized-route` skill and don't touch geometry, elevation, or
physics code. Colours stay in `data/palette.rs` / `data/biomes.json`.

## Phase 5 — Test compaction (low risk to the app, needs care)

1. Before starting, save the full case list: `cargo test -- --list > docs/refactor/tests_before.txt`.
2. In the large test modules, turn runs of near-identical `#[test]` functions into
   table-driven tests:
   ```rust
   #[test]
   fn should_classify_zone_boundaries() {
       let cases = [
           (200, 180, PowerZone::Tempo),     // 90 %
           (200, 182, PowerZone::Threshold), // 91 %
           // ... every former test becomes one row, comment keeps the old test name
       ];
       for (ftp, watts, want) in cases {
           assert_eq!(zone(ftp, watts), want, "ftp={ftp} watts={watts}");
       }
   }
   ```
   Keep the old test name as a comment on each row so failures can still be traced.
3. Move repeated fixture setup into shared helpers in each test module (or a `testutil` module
   under `#[cfg(test)]`).
4. Don't merge tests that check *different* behaviours just because they look alike, and
   don't merge property tests (`proptest`). Leave `proptest-regressions/` unchanged.
5. After each module: `cargo test` must pass, and a diff of the rows against
   `tests_before.txt` must account for every removed test name.

## Phase 6 — Wrap-up

1. Re-run all of Phase 0's measurements and write `docs/refactor/result.md` with
   before/after for: total lines, non-test lines, test lines, test cases (as rows), jscpd duplication %.
2. Run `cargo audit` and `cargo deny check` (CLAUDE.md §5.5).
3. This is not a release. Don't bump the version or stamp the README. If the rider then
   releases, follow CLAUDE.md §4.5 as usual.
4. List what was considered and left alone, with the reason for each.

---

## Phase 7 — Declarative UI pilot: `SummaryPage` to Blueprint + composite template

**Why:** every page is built by hand in Rust. Today there are no `.ui`/`.blp` files and no
GObject subclasses, and there are ~1,400 `.clone();` calls and ~277 `RefCell`s in `ui/` to move state into
closures. That is a large share of the UI's 42,000 lines, and it is where the dialog leak bugs
that CLAUDE.md §2.4 warns about come from. With templates, properties, and template callbacks, most of that
code goes away.

**Why `summary.rs` as the pilot:** it's self-contained (~810 non-test lines). It has one
constructor (`SummaryPage::new(on_done)`), a handful of `show_*`/`update` methods, three
button handlers, and a few `Rc<RefCell<>>` fields. It's used in only three places:
`ui/window/mod.rs`, `ui/window/session.rs`, and `ui/pages/calendar/screenshots.rs`.
It has no BLE, physics, or 3D code.

This phase is a **pilot**: the goal is to prove the pattern and measure it. Don't convert
other pages in the same session.

### 7.1 Decide the toolchain (stop and ask the rider)

Blueprint needs `blueprint-compiler` at build time. It is **not** in `org.gnome.Sdk`, so:

- **Option A (recommended): Blueprint.** Add `blueprint-compiler` as a module in
  `io.github.rorynuijens.Cycle.yml` (built before `cycle`), and install it in the dev
  toolbox (`dnf install blueprint-compiler`). In `build.rs`, run
  `blueprint-compiler batch-compile` on `data/ui/*.blp` into `$OUT_DIR/ui/` *before*
  `glib_build_tools::compile_resources`, and add that output dir as a resource source.
  Add `cargo:rerun-if-changed=data/ui/`.
- **Option B: plain GtkBuilder `.ui` XML.** No new build tool; more verbose to read.
  Same Rust code otherwise.

Make sure the Blueprint output uses only widgets/properties available in GTK 4.12 /
libadwaita 1.5 (CLAUDE.md §6, point 5). Check any property the compiler warns about.

### 7.2 Convert

1. Create `data/ui/summary-page.blp` (or `.ui`) describing the whole static layout:
   header bar, metric columns, zone swatches container, compliance group, export banner
   (`adw::Banner`), export and done buttons. Keep every CSS class, spacing value (6 px
   grid), tooltip, and accessible label exactly as they are now. Register it in
   `data/cycle.gresource.xml` under `/io/github/rorynuijens/Cycle/ui/summary-page.ui`.
2. Make `src/ui/pages/summary/` a module with `mod.rs` (public wrapper) and `imp.rs`
   (subclass):
   - `#[derive(Default, gtk::CompositeTemplate, glib::Properties)]`
     `#[template(resource = "/io/github/rorynuijens/Cycle/ui/summary-page.ui")]`
   - `#[template_child]` for every widget the code updates (labels, rows, banner, buttons).
   - Plain `RefCell<…>` fields in `imp` replace the `Rc<RefCell<…>>` fields
     (`last_session`, `last_athlete`, `compliance_rows`, export folder URI). The object
     owns them, so no `Rc` and no clones are needed.
   - Button handlers become `#[template_callbacks]` methods, wired with `on_clicked`
     in the template, so they get `&self` and capture nothing.
   - Subclass `adw::Bin` (or `gtk::Box`, whichever matches the current root widget's layout
     behaviour) so `widget()` can return `self.upcast_ref()` and callers still work.
3. Replace the `on_done: impl Fn()` constructor argument with a custom signal `done`
   (`glib::subclass::Signal`). Keep a thin `SummaryPage::new(on_done)` that creates
   the object and connects the closure, so the three call sites don't change in this phase.
   Inside `on_done`, capture any window **weakly** (CLAUDE.md §2.4).
4. Keep `metric_column` and `zone_swatch` in Rust if they're created dynamically. Zone
   colours still come from `ui/widgets/zone_color.rs`, never from the template or CSS.
5. Keep `segments_the_ride_reached` and its six tests unchanged. They're pure logic; move them
   next to the wrapper or into `training/` only if that needs no other changes.
6. Every public method (`show_progression`, `show_integrity`, `show_ftp_test_result`,
   `show_rpe_icon`, `update`, `widget`) keeps its signature. Callers must compile
   without changes.

### 7.3 Verify

- Gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Build the Flatpak once (`flatpak-builder`) to prove `blueprint-compiler` resolves there.
- **Screenshot check:** `calendar/screenshots.rs` already builds `SummaryPage` for
  screenshots. Make screenshots before and after (light and dark, 900 px and 1400 px) and
  compare them. They should be identical apart from anti-aliasing.
- **Manual check (ask the rider):** finish a workout and a route ride, and check that the
  summary shows the right numbers, zones, compliance rows, progression card, integrity notice,
  FTP test result, and RPE icon. Test both export paths (banner button and export button) and *Done*.
- **Leak check:** open and close the summary 20 times with
  `GTK_DEBUG=interactive`. In GTK Inspector → Objects, the `SummaryPage` instance count
  shouldn't grow.

### 7.4 Report, then decide

Write `docs/refactor/blueprint-pilot.md` with:
- Rust lines before/after for `summary`, plus `.blp` lines (counted separately and honestly)
- `.clone()` and `RefCell` counts before/after in the module
- build-time impact, and any GTK 4.12 / adw 1.5 snags
- a recommendation: continue or stop. If continuing, the rollout order is:
  1. `summary.rs` (this pilot)
  2. **the calendar pages** (`ui/pages/calendar/`): second because the rider plans in them
     every day and they have the worst clone-and-capture wiring (one reload closure clones
     about fifteen values twice). If Phase 3 has already merged their shared builders,
     convert those builders first, then `week.rs`, `month.rs`, `detail.rs`, `dialogs.rs`.
  3. `coaching/` and `dashboard.rs` (the rest of the planning surface)
  4. `onboarding.rs`, `library/`, `preferences/`
  5. the players last, after Phases 1–2 have merged them

**Stop.** The rider decides whether to roll the pattern out.

Expected pilot result: roughly 30–50 % fewer Rust lines in `summary`, some of them moved into
`.blp`. Net reduction is smaller but real, and the module ends up with no strong-capture closures.

---

## Phase 8 — Planning works without AI (behaviour check, small fixes allowed)

**Why:** the rider plans training in this app. The calendar and the program rules read the
morning brief's verdict (`CoachVerdict`, via `ui/brief_store.rs`) to decide when to ease off.
Planning must not silently change, stall, or break when there's no Anthropic key, the key is
refused, the API is down, or the brief is still loading. AI may *add* to a plan; it must
never be needed to *have* one.

**Rules for this phase:** a behaviour change is allowed only where the app currently fails or
degrades without AI. Each such change gets its own commit, a test, and a line in the report.

1. **Map it.** List every place planning code reads AI output: `CoachVerdict`,
   `BriefStore`, `ai_cache`, `ai::coach`, `ai::naming`, and `icu_workouts` used for AI
   suggestions. For each one, write what happens when the value is missing, stale, or an error.
   Start with `training/program.rs` (`plan_view`, `suggest`), `training/rollover.rs`,
   `ui/pages/calendar/mod.rs` (the reload passes `verdict.get()` into `plan_view`),
   `ui/pages/coaching/`, and `ui/pages/dashboard.rs`.
2. **Write the missing tests** in `training/` (pure, no GTK): for each planning rule, assert
   that the result with `CoachVerdict::default()` (no brief) matches the no-AI behaviour the rider
   would expect. Name them `should_<…>_when_no_brief`. The rules to cover are building
   a block, rollover, matching a ride to a planned day, and adjusting after missed or extra load.
3. **Fix what fails.** Typical fixes: treat "no verdict" as neutral rather than "rest";
   never block a calendar reload on the brief; show the plan first and add the AI line when
   it arrives; keep a refused or expired key's error to one `adw::Toast` or `adw::Banner`, never
   an empty planning page.
4. **Manual check (ask the rider):** remove the Anthropic key in Preferences, restart, and use
   the calendar, coaching page, and dashboard. Schedule, ride, and let a block roll over. Then
   do it again with a deliberately wrong key. Nothing should be empty, stuck, or blocked.
5. **Report:** every dependency found, what it did before and after, and the tests added.

## Phase 9 — Garmin Connect first, Intervals.icu optional (feature change)

**Why:** in October 2026 Garmin changed its developer terms on sharing users' data with
services it hasn't approved. Intervals.icu warned that it would probably have to stop
passing Garmin-originated data (activities, FIT files, streams, wellness, including history)
through its own API. On 14 October its developer said the requirements weren't new and
were part of Garmin reopening its developer program, so nothing is confirmed. But this app
reading Garmin data *through* Intervals.icu is exactly the path at risk. Reading it
directly from Garmin's Personal API isn't. The rider has decided to move to Garmin.

**Where things stand at 0.23.1:**
- Incoming data already prefers Garmin. `ui/brief_store.rs::sync_activities` and the
  calendar's sync button use Garmin when a Garmin key is set, and fall back to Intervals.icu
  otherwise. Garmin data lands in the same tables as Intervals.icu data, with ids prefixed `g`.
  `garmin::sync` already folds duplicate Intervals.icu copies into the Garmin ones.
- These still depend on Intervals.icu specifically:
  - **ride upload**: `ui/window/session.rs` → `intervals::upload_fit_activity`
  - **workout library download** for AI suggestions: `ui/preferences/integrations.rs` →
    `intervals::fetch_workouts`; read back as `icu_workouts` in `ai/brief/input.rs` and
    `ui/pages/coaching/data.rs`
  - **calendar detail extras**: `ui/pages/calendar/detail.rs` →
    `intervals::fetch_combined_activity_data`
  - **stream backfill without Garmin**: `intervals::backfill_streams` (calendar, brief)
  - **onboarding**: offers Intervals.icu setup (`ui/pages/onboarding.rs`)
  - **naming**: `IntervalsActivity`, `load_intervals_*`, `icu_*` are used for *all* outside
    activities, Garmin's included
- Garmin's Personal API is **read-only**. There is no way to upload a ride to Garmin
  Connect through it. The personal key also **expires** (`settings::garmin_key_expires`).

**Rules for this phase:**
- **Never delete local history.** Activities, wellness, and streams already stored from
  Intervals.icu stay in SQLite, whatever happens to Intervals.icu's API. That local copy is
  the real hedge against both services.
- **No schema change without asking.** Renaming tables or columns needs a migration and a
  backup (`data/backup.rs`). Propose it in the report; don't do it unasked.
- Intervals.icu stays as an **optional** integration until the rider says to remove it.
  Uploading the app's *own* rides there isn't Garmin data, and Intervals.icu said uploads are
  likely unaffected.

### 9.1 Garmin as the default

1. Onboarding: offer Garmin Connect first and Intervals.icu as "optional, for uploading rides".
2. Preferences → Integrations: Garmin group first. Describe Intervals.icu as optional upload and
   history. Keep `adw::PreferencesGroup`/`adw::ActionRow` (CLAUDE.md §1.1).
3. UI text: replace Intervals.icu-first wording (e.g. "Set your Garmin key, or your
   Intervals.icu Athlete ID and key, first", "Sync activities from Garmin Connect, or from
   Intervals.icu without a Garmin key") with Garmin-first wording. Show the list of changed
   strings to the rider before committing.
4. Make the Garmin key's expiry hard to miss: an `adw::Banner` on the dashboard a week
   before it expires, with a button to Preferences. Check what `expiry_notice` does now
   first; extend it, don't duplicate it.

### 9.2 Everything works with Intervals.icu not configured

For each Intervals.icu-only path above, make sure that with no Intervals.icu credentials there are
no error toasts, no empty cards, and no "configure Intervals.icu" nags:
- **Calendar detail:** where `fetch_combined_activity_data` fills in extras, use the Garmin
  activity's FIT file or `garmin::sync::fetch_streams_json` for Garmin activities. Hide the
  extras quietly when neither is available.
- **AI suggestions:** must work from the local library alone. `icu_workouts` becomes an
  optional extra, and an empty list is the normal case (ties into Phase 8).
- **Fitness, progression, dashboard, player** (`load_intervals_activities` /
  `load_intervals_tss_pairs` callers): already read local tables, so check that they read
  Garmin rows the same way.
- Add tests in `ai/garmin/sync/tests.rs` and `data/db/` for: Garmin only; neither service; and
  both services, where Garmin wins and Intervals.icu copies fold in (some of this may exist already).

### 9.3 Getting rides out without Intervals.icu

Garmin offers no upload API to personal users, so be honest in the UI:
1. After a ride, the summary's export already writes a FIT file. Add a secondary action,
   "Upload to Garmin Connect…", that saves the FIT (if it isn't saved yet) and opens Garmin
   Connect's manual import page in the browser (`gtk::UriLauncher`, available
   since GTK 4.10). Verify the current import page URL before
   hard-coding it, and keep it in one constant.
2. Keep the existing ride card (`ui/ride_card.rs`) next to it, since it was built for this flow.
3. Keep automatic Intervals.icu upload as an option, off by default for new setups, and
   unchanged for riders who already have it on.

### 9.4 Naming (propose, then ask)

`IntervalsActivity`, `load_intervals_*`, and `icu_*` now describe outside activities from
any source. Propose a Rust-only rename (e.g. `ExternalActivity`, `load_external_*`) that
keeps table and column names, with an estimate of how many files it touches. **Don't
start the rename without the rider's go-ahead.** It's a large mechanical change and
conflicts with any refactor phase in progress, so it's best done between phases.

### 9.5 Verify and report

- Gate as usual, plus a manual run (ask the rider) with: Garmin only; Garmin + Intervals.icu;
  neither. Each time, open the calendar, fitness, coaching, dashboard, summary, and Preferences.
- README: update the integrations section and "Current limitations" at the next release
  (CLAUDE.md §4.5). Don't stamp it in this phase.
- Report: what moved to Garmin, what is still Intervals.icu-only and why, and the rename proposal.

---

## Expected result

| | Baseline (0.23.1) | Realistic target |
|---|---|---|
| Total Rust lines | 109,620 | ~95,000–100,000 |
| Non-test lines | ~72,900 | ~60,000–65,000 |
| Test lines | ~36,900 | ~32,000–35,000 (same cases) |
| Features | all | all |

A 75 % cut is not reachable without removing features or tests; this plan doesn't try to.
