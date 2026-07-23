# FTP Detection — "FTP check-in" specification

Status: **specced, not yet built** · Author: 2026-07-23

Automatically suggest FTP adjustments by monitoring workout completions,
in the spirit of TrainerRoad's AI FTP Detection — but deterministic,
explainable, and local. The app **never changes FTP silently**: every
adjustment is a suggestion with visible evidence and a one-tap Accept.

---

## 1. Why power curves are not enough

Classic FTP estimation (Intervals.icu eFTP, Golden Cheetah) fits a power
curve over best recent maximal efforts. ERG mode invalidates this for
indoor-only riders: the trainer caps power at the prescribed target, so
the recorded power curve reflects what workouts *asked for*, not what the
rider *could do*. Detection must therefore lean on **completion quality**:
compliance, struggle events, RPE, and heart-rate response — signals Cycle
already records or can start recording.

## 2. Data model changes (all additive)

| Change | Purpose |
|---|---|
| `DataPoint.target_watts: Option<u32>` (`#[serde(default)]`) | The target in force at each recorded second, written by the workout player from the engine. Old JSON deserialises with `None`; route/SIM rides have no target and are excluded from compliance analysis automatically. |
| `sessions.ftp_watts INTEGER` (additive `ALTER`) | FTP at ride time. Required to interpret compliance/zones after FTP changes. |
| New table `ftp_history(id, date, ftp_watts, source, note)` | Audit trail and cooldown bookkeeping. `source ∈ {'manual','suggestion','ramp_test'}`. Preferences edits also insert rows. |

**Phase-1 consequence:** the detector can only read sessions recorded
*after* target capture ships. Ship capture early so evidence accumulates
from the first ride back.

## 3. Evidence extraction (`training/ftp_detect.rs`, pure Rust)

Analysis window: the last **28 days** of sessions that have targets.
Per session, derive a `SessionEvidence`:

- **Hard segments** — contiguous runs of seconds with
  `target ≥ 0.91 × session_ftp` (threshold and above; the 91% boundary
  matches the app's zone model).
- **Compliance** per hard segment: `mean(power) / mean(target)`.
- **Struggle flag** (the ERG "spiral of death"): within a hard segment, a
  run of ≥ 10 s where `cadence < 0.8 × session median cadence` **and**
  `power < 0.95 × target` → segment *failed*. A session ended before 90%
  of planned duration whose last segment was hard also counts as a
  failure.
- **RPE** (1–10, if recorded).
- **HR drift** for steady hard segments ≥ 8 min:
  `mean(HR second half) / mean(HR first half)` — a proxy for aerobic
  decoupling; falling drift across weeks indicates rising fitness.
- A session is a **hard-evidence session** if it contains ≥ 10 min total
  of hard-segment time. Other rides contribute only HR-drift trend.

## 4. Suggestion rules (deterministic, first match wins)

Window summary: `n_hard` (hard-evidence sessions), `fail_rate`
(failed / total hard segments), `avg_compliance` (duration-weighted),
`avg_rpe_hard` (mean RPE over hard sessions, needs ≥ 2 values),
`drift_trend` (this window vs previous window).

Preconditions: `n_hard ≥ 3`, cooldown satisfied (§5). Then:

1. **Down** if `fail_rate ≥ 0.25`, or `avg_rpe_hard ≥ 8.5` with
   `fail_rate ≥ 0.10`.
   Magnitude: `clamp(2 + 10 × fail_rate, 2, 5)` %.
2. **Up** if `fail_rate = 0` and `avg_compliance ≥ 0.98` and
   `avg_rpe_hard ≤ 6.0`.
   Magnitude: 2%, +1% if `avg_rpe_hard ≤ 5.0`, +1% if drift improved by
   ≥ 2 percentage points; cap 5%.
3. **Up (cross-check)** if `avg_rpe_hard ≤ 6.5` and a synced
   Intervals.icu eFTP ≥ `1.03 × ftp`: suggest `min(eFTP, 1.05 × ftp)` —
   covers outdoor rides where real maximal efforts exist.
4. Otherwise: **"FTP looks right"** — the check-in still renders, with
   evidence, so the monthly rhythm is visible.

Output: `FtpSuggestion { new_ftp, delta_pct, direction, evidence: Vec<String> }`
where each evidence string is a human sentence
("4 of 4 threshold sessions completed at average RPE 5").

## 5. Guard rails

- |Δ| capped at **5% per suggestion**; suggested FTP clamped to 50–500 W
  (never trust stored data blindly — CLAUDE.md §5.2).
- Cooldown: no *up* suggestion within 21 days of any `ftp_history` entry;
  *down* allowed after 7 days (overtraining protection beats rhythm).
- Dismissing a check-in snoozes it 14 days (settings key).
- Accept updates the athlete profile, inserts `ftp_history`, shows a
  toast. Nothing is ever changed without the tap.

## 6. Surfaces

- **Fitness page — "FTP check-in" card**, shown when a suggestion (or
  monthly "looks right") is due: suggested value, delta, evidence lines,
  `Accept <N> W` (suggested-action) / `Not now`. Zone colours only where
  a colour means a zone; otherwise standard Adwaita.
- **Preferences → Athlete**: FTP row subtitle becomes
  "Last updated <date> · <source>"; manual edits log to `ftp_history`.

## 7. Ramp test (phase 3)

Built-in "Ramp Test" workout for when heuristics disagree or the user
wants ground truth: 5 min warm-up, then +6% FTP per minute until failure
(detected by the struggle detector or the user ending the test).
`FTP = 0.75 × best rolling 60 s power`, presented via the same accept
flow (`source = 'ramp_test'`). Player runs it in test mode: open-ended,
step count instead of remaining time.

## 8. Module layout & threading

- `training/ftp_detect.rs` — evidence extraction, window summary,
  suggestion rules. Zero GTK, zero async; fully unit-testable.
- `data/db.rs` — `ftp_history` CRUD; windowed session query returning
  `(id, ftp_watts, rpe, data_points_json)`.
- Analysis runs on the tokio runtime via `spawn_to_main` (session JSON
  can be megabytes; never parse on the GTK main thread).
- `ui/pages/player.rs` — one-line change: stamp `target_watts` into each
  recorded `DataPoint`.

## 9. Testing

Synthetic-session builder (targets + power/cadence/HR traces + RPE), then:

- perfect compliance + RPE 5 → up suggestion, magnitude 3–4%
- one spiral-of-death segment in three → down suggestion
- `n_hard < 3` → no suggestion regardless of quality
- cooldown and snooze guards hold
- pre-capture sessions (no targets) are ignored
- magnitude caps at 5% even for extreme inputs
- zone boundary: target at exactly 90% vs 91% of FTP classifies
  correctly (boundary tests per CLAUDE.md §3)

## 10. Phasing

1. **Capture** (ship first, invisible): `target_watts` in DataPoint,
   `sessions.ftp_watts`, `ftp_history` + Preferences logging.
2. **Detector + check-in card** — needs ≥ 3 hard sessions of new-format
   data, i.e. usable ~2 weeks after riding resumes.
3. **Ramp test** workout + test mode.
4. **Polish**: Intervals.icu eFTP cross-check, optional Claude-written
   explanation prose on the check-in card (facts stay deterministic).
