# Plan rust-05-sqlite-stores: Swift-Compatible UsageAggregationCache, Legacy Upgrade/Backfill, DashboardStateStore, and DirectoryNameStore

- **planId**: `rust-05-sqlite-stores`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/cache`
  - `crates/ccusage-core/tests/sqlite_stores_tests.rs`
  - `tmp/cargo-target/rust-05-sqlite-stores`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-05-sqlite-stores`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement all SQLite 3 persistence stores using bundled `rusqlite` with 100% on-disk compatibility for existing Swift-created databases:
1. `UsageAggregationCache` (`aggregates-<machineId>.sqlite3`), including legacy `aggregates.sqlite3` atomic rename migration, `MachineCacheRecovery` marker reconciliation, `sourceConfigurationFingerprint` + `sourceGeneration` fencing, additive local-only `antigravity_coverage_ranges`, and strict total-conserving legacy provenance backfill (`DESIGN-LEGACY-01` & `DESIGN-CACHE-07`).
2. `DashboardStateStore` and `DashboardDirectoryNameStore` (`dashboard-state.sqlite3`), including prepared/committed/rollback deletion barriers and startup registry reconciliation.

Existing Swift references to imitate:
- `Sources/AppCore/AggregationCache.swift`: exact table schemas, `PRAGMA journal_mode = DELETE`, `PRAGMA user_version = 1`, `adoptLegacyFingerprintIfNeeded`, `normalizedRanges`, `pending_range_jobs`, `beginSourceAttempt`, `staleGeneration`.
- `Sources/AppCore/CacheLifecycle.swift`: `LocalCacheMigrator` (atomic rename from `aggregates.sqlite3` -> `aggregates-local.sqlite3` with no copy fallback, conflict warning when both exist) and `MachineCacheRecovery` (atomic staged clear + recovery marker).
- `Sources/AppCore/DashboardStateStore.swift`: `dashboard_state` UTF-8 JSON BLOB table.
- `Sources/AppCore/DashboardDirectoryNameStore.swift`: `dashboard_directory_names`, `dashboard_directory_name_deletions` (prepared/committed/rollback phases), `reconcileMachineRegistry`, stale-authorization rejection on active deletion marker.

## Non-Goals

- Do NOT reset or rewrite existing Swift SQLite caches destructively.
- Do NOT alter remote SSH cache semantics (`antigravity_coverage_ranges` is local-cache only).
- Do NOT rewrite stored `session_metrics` timestamps.
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/cache/mod.rs`**: re-exports `aggregation_cache`, `lifecycle`, `dashboard_state_store`, `directory_name_store`.
2. **`crates/ccusage-core/src/cache/aggregation_cache.rs`**:
   - Exact Swift-compatible schema: `PRAGMA journal_mode = DELETE`, `PRAGMA synchronous = NORMAL`, `PRAGMA user_version = 1`, tables `cache_metadata`, `daily_metrics`, `session_metrics`, `coverage_ranges`, `pending_range_jobs`, `directory_coverage_ranges`, plus additive local-only `antigravity_coverage_ranges(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day))` and additive local-only nullable `source_day TEXT` column on `session_metrics` (with index `session_metrics_source_day_idx`).
   - Idempotent additive column migrations (`cached_from`, `source_configuration_fingerprint`, `directory`, `effort`, and local-only `session_metrics.source_day TEXT` leaving existing legacy rows with `source_day = NULL` and unmodified timestamps) and pre-`user_version = 1` one-time local `DELETE FROM directory_coverage_ranges` + `PRAGMA user_version = 1`.
   - `adopt_legacy_fingerprint_if_needed`: pre-upgrade `NULL` fingerprint is adopted, NOT purged.
   - `source_configuration_fingerprint` + `source_generation` fence rejecting stale-generation writes with `AggregationCacheError::StaleGeneration`.
   - **UTC 15-minute bucketing across local-day boundaries (`source_day` additive contract)**:
     - `events_to_session_buckets(events, start, end, machine)` validates raw event instants strictly against `[start, end)` and computes exact UTC 15-minute flooring `floor(unix_seconds / 900) * 900` (`design-rust-tauri-migration.md:123`) without rejecting events whose UTC-floored bucket lands before `start` when local midnight falls inside a 15-minute bucket.
     - `merge_day` for local caches validates session row timestamps against `[floored_start, end)` where `floored_start = ((start.floor() as i64).div_euclid(900) * 900) as f64`, persists `source_day = Some(day)` on every inserted row, and scopes day deletions (`Uncovered`, `Complete` partition replacement, and `replace_agy`) strictly to `(source_day = ?day OR (source_day IS NULL AND timestamp >= ?start AND timestamp < ?end))` so adjacent-day merges never lose or duplicate shared-bucket contributions and never touch legacy `source_day IS NULL` cached-only history from another day.
     - `read_sessions` / `session_metrics(start, end)` / `load(...)` coalesce rows sharing identical `(timestamp, agent, model, directory, effort, data_quality)` via `group_sessions`; `session_metrics_for_day(day, start, end)` queries `(source_day = ?1 OR (source_day IS NULL AND timestamp >= ?2 AND timestamp < ?3))` for local caches (and `timestamp >= ?1 AND timestamp < ?2` for remote caches), coalesced via `group_sessions`.
   - **Per-day legacy vs native coverage classification (`local` only)**:
     - `Uncovered` (`d` not in `coverage_ranges`)
     - `AggregateCoveredProvenanceUnscanned` (`d` in `coverage_ranges`, not in `directory_coverage_ranges`)
     - `LegacyCoveredNeedsAntigravity` (`d` in `coverage_ranges` and `directory_coverage_ranges`, not in `antigravity_coverage_ranges`)
     - `NativeComplete` (`d` in all three coverage tables)
   - **`evaluate_legacy_partition_completeness`** (shared with `rust-09` for `DESIGN-CACHE-07` & `DESIGN-ROLLING-10`):
     - Returns `LegacyPartitionDecision::Complete` iff scanned raw events for `(d, agent, model)` are non-empty AND summed integer `(input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)` **exactly equals** cached `daily_metrics` token totals (with at least one token total `> 0`, or both cached and raw costs `== 0`).
     - Returns `LegacyPartitionDecision::PartialOrUnverifiable` when scanned events are non-empty but fail the exact match (e.g., `$4` surviving vs `$10` cached).
     - Returns `LegacyPartitionDecision::Absent` when 0 events are scanned for `(d, agent, model)`.
   - **Atomic single-transaction merge/backfill (`BEGIN IMMEDIATE ... COMMIT`)**:
     - `Uncovered`: atomically replace rows for `d`, mark `coverage_ranges`, `directory_coverage_ranges`, `antigravity_coverage_ranges`.
     - `AggregateCoveredProvenanceUnscanned`: NEVER alter cached Claude/Codex `daily_metrics`. For each Claude/Codex `(agent, model)`: if `Complete`, replace its `session_metrics` partition with directory/effort-attributed 15m buckets whose cost and 4 token totals conserve `daily_metrics` exactly (remainder on final event); if `PartialOrUnverifiable` or `Absent`, retain existing cached `session_metrics` rows untouched! Simultaneously replace any `agent == "agy"` rows in `daily_metrics` and `session_metrics` for `d`, and mark `directory_coverage_ranges` + `antigravity_coverage_ranges` in the same transaction.
     - `LegacyCoveredNeedsAntigravity`: retain ALL cached Claude/Codex `daily_metrics` and `session_metrics` untouched; atomically delete any prior `agent == "agy"` rows for `d`, insert freshly scanned `agent == "agy"` rows, and mark `antigravity_coverage_ranges` in one transaction.
     - Failure/interruption rolls back the entire transaction; snapshot publication happens only after commit; retry produces identical totals with zero duplication.
3. **`crates/ccusage-core/src/cache/lifecycle.rs`**:
   - `LocalCacheMigrator`: atomic rename `aggregates.sqlite3` -> `aggregates-local.sqlite3` after WAL checkpoint + close + `0600` perms (no copy fallback; destination authoritative if both exist with sanitized conflict warning; unsafe/invalid legacy ignored).
   - `MachineCacheRecovery`: atomic staged clear + recovery markers reconciling interrupted clears on startup to either complete prior state or complete empty state before opening the cache.
4. **`crates/ccusage-core/src/cache/dashboard_state_store.rs`**:
   - `DashboardStateStore`: exact `dashboard_state` UTF-8 JSON BLOB table in `dashboard-state.sqlite3` (`0600` inside `0700` dir).
5. **`crates/ccusage-core/src/cache/directory_name_store.rs`**:
   - `DashboardDirectoryNameStore` implementing `DirectoryNameLifecycle` (`crates/ccusage-core/src/types.rs`): `dashboard_directory_names`, `dashboard_directory_name_deletions` (prepared/committed/rollback phases), name validation (trimmed, non-empty to set, null/empty/whitespace to clear, no control chars, `<= 200` Unicode code points), rejecting writes when a machine has an active deletion marker (`machine_not_found`), and startup `reconcile_machine_registry(machine_ids)` matching `architecture.md:458-493`.

## Key Invariants and Pitfalls

- **Decimal storage**: Store `cost_usd` as exact decimal text (`Decimal::to_string()`), never SQLite `REAL`.
- **Legacy timestamp preservation**: Store and read `session_metrics.timestamp` as Unix seconds `f64` matching Swift `timeIntervalSince1970`, never rewriting existing rows.
- **No partial overwrite / no unsupported attribution**: In `DESIGN-CACHE-07`, a surviving `$4` log on a `$10` legacy day MUST retain the `$10` cached `session_metrics` rows untouched—never replace with `$4`, never add `$4 + $10 = $14`, and never scale `$4` up to `$10` under the surviving log's directory!
- **Remote caches**: Never create or consult `antigravity_coverage_ranges` or `session_metrics.source_day` for remote machine IDs (`machine_id != "local"`).

## Test Cases (`crates/ccusage-core/tests/sqlite_stores_tests.rs`)

- Open synthetic Swift-era `PRAGMA user_version = 1` DB (without `antigravity_coverage_ranges`) where `coverage_ranges` and `directory_coverage_ranges` cover yesterday with cached-only Claude usage (no log on disk) + Antigravity usage yesterday -> retains cached Claude, adds Antigravity once, marks `antigravity_coverage_ranges`, yields identical totals after reopen and simulated rollback/retry (`DESIGN-LEGACY-01`).
- Open legacy `coverage_ranges`-only DB (`$10` daily + `$10` session for one Claude model):
  - Complete surviving logs (exact token match) -> replaces session partition with attributed 15m buckets conserving exact `$10` and all 4 token counts.
  - Partially missing same-model logs (`$4` surviving vs `$10` cached) -> retains `$10` daily and `$10` session/token totals untouched with zero duplication and no unsupported directory attribution across reopen and retry (`DESIGN-CACHE-07`).
  - Fully missing logs (0 surviving) -> retains `$10` daily and `$10` session totals untouched.
- Simulated SQLite failure mid-backfill -> rolls back both rows and coverage markers; retry commits cleanly with identical totals.
- Adjacent local days where local midnight falls inside a UTC 15-minute bucket (e.g., `UTC+00:07:30`) -> `events_to_session_buckets` accepts valid events whose UTC-floored bucket `B < start`, `merge_day` for both days preserves both contributions (`session_metrics` over both days coalesces `B` to the exact sum; `session_metrics_for_day` returns each day's exact share), replacing/re-merging either day neither loses nor duplicates the adjacent day's contribution, and a legacy `source_day IS NULL` cached-only row in `B` on `day1` survives `day2`'s merge and retry untouched.
- `sourceConfigurationFingerprint` adoption for legacy `NULL`, purge on mismatch, and `StaleGeneration` rejection.
- `LocalCacheMigrator` atomic rename, both-exist destination-wins warning, and `MachineCacheRecovery` interrupted-clear recovery.
- `DashboardDirectoryNameStore` set/clear/reopen, validation (control chars, >200 code points), prepared/committed/rollback marker transitions, concurrent write rejection during deletion, and startup reconciliation for machine-id reuse.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --edition 2021 --check crates/ccusage-core/src/cache/*.rs crates/ccusage-core/tests/sqlite_stores_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-05-sqlite-stores cargo test --locked -p ccusage-core --test sqlite_stores_tests
```

Done when both commands exit `0` and all SQLite compatibility, legacy backfill, local-day boundary `source_day` persistence/coalescing, rollback/retry, and directory-name lifecycle tests pass. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-05-sqlite-stores` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 2 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-05-sqlite-stores`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Amendment 2026-11-27 (blocked-wave-scope-or-contract, round 2)

- **Additive local-only `source_day` contract for UTC 15-minute buckets crossing local-day boundaries**: Resolves the `rust-05-sqlite-stores` blocker without changing `design-docs/specs/design-rust-tauri-migration.md:123`, `types.rs`, remote cache schemas, or legacy timestamps:
  - Idempotently add nullable `source_day TEXT` (and index `session_metrics_source_day_idx`) to `session_metrics` for `machine_id == "local"` only, keeping `PRAGMA user_version = 1` and leaving all existing legacy rows with `source_day = NULL` and unmodified `timestamp`.
  - `events_to_session_buckets` validates raw event instants in `[start, end)` and computes exact UTC 15-minute flooring `floor(unix_seconds / 900) * 900` without rejecting `floor < start`. `merge_day` validates local session rows in `[floored_start, end)` (`floored_start = ((start.floor() as i64).div_euclid(900) * 900) as f64`), writes `source_day = Some(day)`, and scopes all local day-replacement deletions (`Uncovered`, `Complete` legacy partition replacement, `replace_agy`) to `(source_day = ?day OR (source_day IS NULL AND timestamp >= ?start AND timestamp < ?end))`.
  - `read_sessions` / `session_metrics(start, end)` / `load(...)` coalesce rows sharing identical `(timestamp, agent, model, directory, effort, data_quality)` via `group_sessions`; expose `session_metrics_for_day(&self, day: &str, start: f64, end: f64)` for day-attributed readback.
- **Workspace edition on standalone `rustfmt` gate**: Updated verification command to `rustfmt --edition 2021 --check crates/ccusage-core/src/cache/*.rs crates/ccusage-core/tests/sqlite_stores_tests.rs` matching `[workspace.package] edition = "2021"`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.

### Implementation progress 2026-11-27

SQLite store modules and initial integration fixtures are in progress. The first isolated test build encountered a cache compilation issue and concurrent errors in `crates/ccusage-core/src/remote/mutation_owner.rs`; the complete log is `tmp/rust-tauri-migration-20261126/rust-05-sqlite-stores/test-attempt-01.log`. Required final behavioral verification and independent review remain pending. Acceptance criteria are not yet complete.

Scoped `rustfmt --check` passed and the isolated locked SQLite integration suite passed 6 tests on the current source (logs: `rustfmt-final-02.log`, `test-final-03.log` in the plan evidence directory). The initial build failure from the sibling remote module cleared; the initial migration fixture was corrected to use an actual cache schema. Implementation remains incomplete: the provenance merge accepts pre-bucketed sessions rather than constructing 15-minute buckets from raw events, and complete-partition conservation, fully absent logs, fingerprint adoption/purge, interrupted-clear recovery, and Swift-era rollback/reopen require dedicated end-to-end fixtures. Formal reviews remain downstream.

Latest update: added raw-event bucketing, complete-partition conservation, fingerprint adoption/purge, and interrupted-clear recovery coverage. Scoped rustfmt and the locked SQLite suite now pass (9 tests; `rustfmt-final-04.log` and `test-final-05.log` in the plan evidence directory). Acceptance remains incomplete: end-to-end fully absent-log and Swift-era rollback/reopen scenarios, raw-event bucket conversion, and all four token categories across complete backfill have not been verified. The bucketing seam rejects local-day boundaries that fall inside a UTC 15-minute bucket; compatibility for such historical time zones remains unresolved. Independent reviews remain downstream.

### Continuation 1

Cache opening now reconciles staged clears and migrates the conventional local legacy database before opening SQLite. Added end-to-end absent-log, Swift-era Antigravity rollback/reopen/retry, four-token conservation, raw-event conversion, and startup recovery/migration fixtures. Scoped rustfmt and the locked test suite pass (14 tests; `continuation-rustfmt-02.log`, `continuation-test-02.log` under the plan evidence directory). Acceptance remains incomplete for historical local-day boundaries inside a UTC 15-minute bucket: exact UTC flooring can produce identical session row keys for adjacent days, while the Swift-compatible schema has no source-day identity and day-scoped deletion cannot distinguish them. Resume after the accepted design authorizes an additive source-day representation or specifies a coordinated adjacent-day replacement contract; preserve existing timestamps and cached history. Formal reviews remain downstream.

### Continuation 2

Rechecked the accepted design and current implementation. UTC 15-minute flooring and day-scoped replacement cannot distinguish contributions from adjacent local days when midnight falls inside a bucket, and the compatible session schema has no source-day identity. The current conversion rejects these valid events; simply removing that guard risks loss or duplication. An accepted design amendment specifying a compatible source-day representation or coordinated adjacent-day replacement preserving cached-only history is required before this plan can be completed. Current scoped rustfmt and locked SQLite tests pass (14 tests; `continuation2-rustfmt.log`, `continuation2-test.log` in the plan evidence directory); these tests do not cover the unresolved boundary. Formal reviews remain downstream.

### Continuation 3: source-day amendment

Implemented the local-only nullable source-day migration and index, day-scoped writes and deletion, UTC bucket boundary handling, and coalesced range and day readback. Added shared-bucket, legacy preservation, repeat merge, reopen, and remote schema regression coverage. Required scoped formatter and SQLite suite are recorded in the plan evidence directory as `continuation3-rustfmt.log` and `continuation3-test.log`; formal reviews remain downstream.

### Test-integrity revision

Addressed RUST05-TI-01 with reverse-order uncovered merges, same-day uncovered and Antigravity replacement, complete legacy partition replacement, and trigger rollback/retry across the shared bucket. Addressed RUST05-TI-02 with an independently constructed populated Swift v1 database and repeated migration assertions. Final scoped verification logs: `ti-final-rustfmt.log` and `ti-final-test.log` in the plan evidence directory. Independent review remains downstream.

### Adversarial revision

RUST05-ADV-01: constrained clear transaction matching to UUID or nonempty single-token suffixes and made post-commit cleanup best effort. Added sibling isolation, interrupted and committed recovery, and sidecar clear coverage. Final verification logs are `adv-final-rustfmt.log` and `adv-final-test.log` in the plan evidence directory. Independent re-review remains downstream.

### Integration review revision

Resolved INT-WAVE8-RUST05-SHADOW-TYPES by re-exporting canonical shared cache types and converting SQLite REAL metadata timestamps to UTC, with regression coverage. Resolved INT-WAVE8-RUST05-CLIPPY with scoped type aliases and clone-free slices. The edition 2021 formatter and offline locked SQLite suite passed (21 tests); scoped Clippy passed with no diagnostics. Logs: `redispatch-final-rustfmt.log`, `redispatch-final-test.log`, and `redispatch-final-clippy.log` in the plan evidence directory. Implementation acceptance criteria are met; independent re-review and workflow finalization remain downstream.
