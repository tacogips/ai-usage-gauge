# Plan rust-02-config-state-logging: ConfigStore, StateStore, ResetWindow, PieChartSelection, and BootstrapLogger

- **planId**: `rust-02-config-state-logging`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/config`
  - `crates/ccusage-core/src/state`
  - `crates/ccusage-core/tests/config_state_logging_tests.rs`
- **sharedPaths**: `[]`
- **artifactRoots**: `[]`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/command.md`

## Intent and Repository Context

Implement configuration loading/validation, mutable state persistence, reset-window/baseline calculation, menu-bar pie-chart selection helpers, and the fail-closed persistent JSONL bootstrap logger in `ccusage-core`.

Existing Swift references to imitate:
- `Sources/AppCore/Configuration.swift`: `AppConfiguration`, `ConfigStore`, `ChartColors`, validation rules, read-only-after-create policy.
- `Sources/AppCore/State.swift`: `AppState`, `StateStore`, atomic `0600` replacement, corrupt-state preservation.
- `Sources/AppCore/ResetWindow.swift`: `ResetWindowCalculator`, `BudgetSummary`, Monday-based ISO weeks, half-open intervals, baseline validation.
- `Sources/AppCore/PieChartSelection.swift`: canonical ordering, deduplication, non-empty menu-bar pie invariant, default `[Budget]`.
- `Sources/AppCore/BootstrapLogger.swift`: lazy file creation, `0700` dir / `0600` regular single-link file fail-closed checks, 10 MiB rotation, 72h retention, advisory `flock`, 16 KiB line cap, redaction, default-dir fallback.

## Non-Goals

- Do NOT modify `Cargo.toml`, `Cargo.lock`, `crates/ccusage-core/src/lib.rs`, or `types.rs`.
- Do NOT invoke external `ccusage` binaries or require `ccusagePath` to exist on disk.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/config/mod.rs` + `configuration.rs`**:
   - `ConfigStore::new(file_path: PathBuf)` with `load_or_create(&self) -> Result<AppConfiguration, ConfigError>` and `load(&self) -> Result<AppConfiguration, ConfigError>`.
   - Defaults: `ccusagePath: null`, `defaultResetTerm: "daily"`, `dashboardPort: 18081`, `dashboardAutostart: true`, `pollIntervalSeconds: 20`, `cacheRetentionDays: 365`, remote retry/timeout defaults matching `Configuration.swift`, default `#RRGGBB` chart colors.
   - Validation: port `1..=65535`, positive `pollIntervalSeconds` and `cacheRetentionDays`, valid remote retry/timeout bounds, valid `#RRGGBB` chart colors, valid `defaultResetTerm`, and when `ccusagePath` is `Some(p)` it must start with `/` (syntax-validated only; NOT required to exist on disk).
   - Read-only after creation: never rewrite an existing file, even if invalid or containing unknown keys (Nix compatibility). On creation, create parent dir (`0700`) and write file (`0600`) atomically.
2. **`crates/ccusage-core/src/config/bootstrap_log.rs`**:
   - `BootstrapLogger::new(primary_dir, fallback_dir, runtime, clock, max_bytes, retention_duration)` — construction is side-effect free.
   - `activate(&self)` validates/creates `0700` user-owned real directory (no symlink), selects fallback (`~/.local/ccusage-gauge/logs`) if primary override fails (recording only sanitized unavailability, never rejected path/exception), acquires advisory `flock`, runs 72h retention cleanup on rotated logs, does NOT create active `ccusage-gauge.jsonl` until first `append(...)`.
   - `append(&self, severity, phase, code, message)`: fail-closed check on `0600` current-user-owned regular single-link file (`nlink == 1`, no symlink), rotates at 10 MiB to `ccusage-gauge-<UTC timestamp>-<monotonic seq>.jsonl`, caps record at 16 KiB single-line JSON, redacts/rejects raw paths/secrets/stderr, never panics or blocks startup if logging fails.
3. **`crates/ccusage-core/src/state/mod.rs` + `app_state.rs`**:
   - `StateStore::new(file_path: PathBuf)` with `load(&self, default_cycle: ResetCycle) -> Result<AppState, StateError>` and `save(&self, state: &AppState) -> Result<(), StateError>`.
   - Preserves all camelCase fields (`budgetUSD`, `budgetMachineIDs`, `resetCycle`, `baseline`, `refreshIntervalSeconds`, `menuBarPieCharts`, `menuBarRatioLabels`, `menuBarShowCost`, `menuBarCostOnly`).
   - Missing file returns defaults; corrupt JSON returns `StateError::Corrupt` without overwriting the file; writes use same-directory temp file + `0600` + atomic rename.
4. **`crates/ccusage-core/src/state/reset_window.rs`**:
   - `ResetWindowCalculator::new(tz: TimeZone)` with `aggregation_interval(&self, cycle: ResetCycle, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>), ResetWindowError>` (exact half-open `[start, end)`), `scheduled_boundary`, `baseline`, `validated_state`, `changing`.
   - Exact boundaries: `Hourly` `[HH:00:00, HH+1:00:00)` local; `Daily` `[00:00:00, next 00:00:00)` local; `Weekly` Monday `00:00:00` through next Monday `00:00:00` local; `Monthly` 1st `00:00:00` through next month 1st `00:00:00` local; `CustomHours(n)` (`n > 0`) `[now - n hours, now)`.
   - `BudgetSummary::new(spent_usd: Decimal, budget_usd: Option<Decimal>)` matching `ResetWindow.swift:76-99` (nonnegative `spent`, `remaining = max(budget - spent, 0)`, `overage = max(spent - budget, 0)`, `usagePercentage = spent / budget * 100`, `visualFraction = min(spent / budget, 1)`).
5. **`crates/ccusage-core/src/state/pie_chart_selection.rs`**:
   - Canonical `PieChartId::menu_bar_cases()`, deduplication/canonical sort, default `[PieChartId::Budget]`, and toggle guard preventing disabling the sole remaining enabled menu-bar icon.

## Key Invariants and Pitfalls

- **DST & timezone transitions**: Use IANA `chrono_tz::Tz` / host local offset rules carefully so local midnight and Monday-start weeks remain accurate across spring-forward and fall-back transitions.
- **`computedAt` invariance**: `ResetWindowCalculator::validated_state` ignores `computedAt` when checking validity, comparing `scheduledBoundaryAt`, `activeBoundaryAt`, `cycle`, `calendarIdentifier`, and `timeZoneIdentifier`.
- **Fail-closed logger security**: Never follow symlinks (`lstat`/`O_NOFOLLOW`), reject hard-linked files (`st_nlink > 1`), reject group/world permissions (`mode & 0o077 != 0`), and never include raw paths or exceptions in log entries.
- **`ccusagePath` compatibility**: Relative `ccusagePath: "ccusage"` MUST fail validation; absolute `ccusagePath: "/nonexistent/ccusage"` MUST pass validation (since local collection no longer executes it).

## Test Cases (`crates/ccusage-core/tests/config_state_logging_tests.rs`)

- Missing config file -> creates default JSON with mode `0600` inside `0700` dir; subsequent load with unknown extra key -> succeeds without rewriting file bytes.
- Relative `ccusagePath`, port `0`, poll interval `0`, invalid `#ZZZZZZ` color -> returns validation error citing config path without modifying file; nonexistent absolute `ccusagePath` -> passes.
- Missing state file -> loads defaults (`menuBarPieCharts = [Budget]`); corrupt JSON state file -> returns error and preserves original bytes untouched.
- Atomic state save/reload -> round-trips `budgetUSD`, `budgetMachineIDs`, `resetCycle` (`CustomHours(240)`), `baseline`, `menuBarRatioLabels`, `menuBarShowCost`, `menuBarCostOnly`.
- `ResetWindowCalculator` with fixed clocks/timezones -> verifies `Hourly`, `Daily`, `Weekly` (Monday start on Sunday/Monday boundary), `Monthly`, `CustomHours(n)`, DST transition day, cycle change clearing/recomputing baseline, and restart persistence.
- `BootstrapLogger` -> verifies side-effect-free construction, lazy file creation, 10 MiB rotation + same-timestamp monotonic suffix, 72h retention boundary (71h59m kept, 72h01m deleted), symlink/hardlink/0644 rejection, primary-to-fallback switch, concurrent append serialization, and redaction.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check crates/ccusage-core/src/config/*.rs crates/ccusage-core/src/state/*.rs crates/ccusage-core/tests/config_state_logging_tests.rs
cargo test --locked -p ccusage-core --test config_state_logging_tests
```

Done when both commands exit `0`, all config, state, reset-window, pie-selection, and bootstrap-logger tests pass, and all files in `writePaths` stay under 1000 lines. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09` to avoid sibling-wave lint interference.)

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
