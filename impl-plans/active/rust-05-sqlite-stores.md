# Plan rust-05-sqlite-stores: Swift-Compatible UsageAggregationCache, Legacy Upgrade/Backfill, DashboardStateStore, and DirectoryNameStore

- **planId**: `rust-05-sqlite-stores`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/cache`
  - `crates/ccusage-core/tests/sqlite_stores_tests.rs`
- **sharedPaths**: `[]`
- **artifactRoots**: `[]`
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
   - Exact Swift-compatible schema: `PRAGMA journal_mode = DELETE`, `PRAGMA synchronous = NORMAL`, `PRAGMA user_version = 1`, tables `cache_metadata`, `daily_metrics`, `session_metrics`, `coverage_ranges`, `pending_range_jobs`, `directory_coverage_ranges`, plus additive local-only `antigravity_coverage_ranges(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day))`.
   - Idempotent additive column migrations (`cached_from`, `source_configuration_fingerprint`, `directory`, `effort`) and pre-`user_version = 1` one-time local `DELETE FROM directory_coverage_ranges` + `PRAGMA user_version = 1`.
   - `adopt_legacy_fingerprint_if_needed`: pre-upgrade `NULL` fingerprint is adopted, NOT purged.
   - `source_configuration_fingerprint` + `source_generation` fence rejecting stale-generation writes with `AggregationCacheError::StaleGeneration`.
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
- **Remote caches**: Never create or consult `antigravity_coverage_ranges` for remote machine IDs (`machine_id != "local"`).

## Test Cases (`crates/ccusage-core/tests/sqlite_stores_tests.rs`)

- Open synthetic Swift-era `PRAGMA user_version = 1` DB (without `antigravity_coverage_ranges`) where `coverage_ranges` and `directory_coverage_ranges` cover yesterday with cached-only Claude usage (no log on disk) + Antigravity usage yesterday -> retains cached Claude, adds Antigravity once, marks `antigravity_coverage_ranges`, yields identical totals after reopen and simulated rollback/retry (`DESIGN-LEGACY-01`).
- Open legacy `coverage_ranges`-only DB (`$10` daily + `$10` session for one Claude model):
  - Complete surviving logs (exact token match) -> replaces session partition with attributed 15m buckets conserving exact `$10` and all 4 token counts.
  - Partially missing same-model logs (`$4` surviving vs `$10` cached) -> retains `$10` daily and `$10` session/token totals untouched with zero duplication and no unsupported directory attribution across reopen and retry (`DESIGN-CACHE-07`).
  - Fully missing logs (0 surviving) -> retains `$10` daily and `$10` session totals untouched.
- Simulated SQLite failure mid-backfill -> rolls back both rows and coverage markers; retry commits cleanly with identical totals.
- `sourceConfigurationFingerprint` adoption for legacy `NULL`, purge on mismatch, and `StaleGeneration` rejection.
- `LocalCacheMigrator` atomic rename, both-exist destination-wins warning, and `MachineCacheRecovery` interrupted-clear recovery.
- `DashboardDirectoryNameStore` set/clear/reopen, validation (control chars, >200 code points), prepared/committed/rollback marker transitions, concurrent write rejection during deletion, and startup reconciliation for machine-id reuse.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check crates/ccusage-core/src/cache/*.rs crates/ccusage-core/tests/sqlite_stores_tests.rs
cargo test --locked -p ccusage-core --test sqlite_stores_tests
```

Done when both commands exit `0` and all SQLite compatibility, legacy backfill, rollback/retry, and directory-name lifecycle tests pass. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09`.)

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
