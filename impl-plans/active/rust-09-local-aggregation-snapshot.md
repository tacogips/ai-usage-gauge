# Plan rust-09-local-aggregation-snapshot: Local Usage Engine, Regenerative 5h Blocks + BlockAnchor, Exact Rolling Totals, and MachineCollector

- **planId**: `rust-09-local-aggregation-snapshot`
- **dependsOn**: `["rust-02-config-state-logging", "rust-03-incremental-scanner", "rust-04-pricing-engine", "rust-05-sqlite-stores", "rust-07-remote-ssh-registry"]`
- **writePaths**:
  - `crates/ccusage-core/src/aggregation`
  - `crates/ccusage-core/tests/aggregation_tests.rs`
  - `target`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `target`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement the core local usage aggregation engine and multi-machine snapshot collection runtime in `ccusage-core`:
1. Local 15-minute chart buckets (`dashboardSessions`), daily metrics (`dashboardMetrics`), regenerative 5-hour billing blocks (`points`) with append-preserving `BlockAnchor` checkpoint reuse (`DESIGN-BLOCKS-02`, `DESIGN-BLOCKS-06`, `DESIGN-BLOCKS-09`), and exact half-open `[activeBoundaryAt, end)` rolling totals with single-day pinned active-boundary scan slot for long `customHours(n)` windows and unified complete/partial/absent legacy partition input precedence (`DESIGN-ROLLING-03`, `DESIGN-ROLLING-08`, `DESIGN-ROLLING-10`).
2. `SnapshotService` (supporting both local native collection + SQLite legacy upgrade/backfill and remote SSH collection), `CostSnapshotMerge`, `MachineSnapshotStore`, `MachineCollector` (implementing `MachineRuntimeReconciler`), `PollingService`, and dashboard historical range loading.

Existing Swift references to imitate:
- `Sources/AppCore/Snapshot.swift`: `selectedPeriodCost`, `initialCoverageStart`, `missingUsageRanges`, `mergeSorted`, `SnapshotService`, `PollingService`, `CostSnapshot::applying`.
- `Sources/AppCore/CostSnapshotMerge.swift`: multi-machine snapshot merging stamping `machine` id, preserving provenance, recomputing totals/budget from eligible rows.
- `Sources/AppCore/MachineCollection.swift`, `MachineSnapshotStore+CollectionLifecycle.swift`, `MachineCollectorRangeLoading.swift`, `MachineRangeLoad.swift`: single-flight per-machine pollers, revision/generation fencing, stale-machine exclusion for current intervals vs historical readability, `latest_event` marker derivation independent of eligibility.

## Non-Goals

- Do NOT invoke local `ccusage` CLI subprocesses or hard-link mirrors for `local`.
- Do NOT implement HTTP routing or CLI rendering here (handled in `rust-10` and `rust-11`).
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/aggregation/mod.rs`**: re-exports `blocks`, `buckets`, `rolling`, `snapshot_service`, `merge`, `collector`.
2. **`crates/ccusage-core/src/aggregation/buckets.rs`**:
   - Floors priced local events to `floor(unix_seconds / 900) * 900` keyed by `(bucket_timestamp, agent, model, directory, effort)` with `UsageDataQuality::Timestamped` -> `dashboardSessions`.
   - Aggregates the exact same priced local events by `(local_calendar_day, agent, model)` with `directory: None` -> `dashboardMetrics`.
3. **`crates/ccusage-core/src/aggregation/blocks.rs`**:
   - Implements `BlockBuilder` + `BlockAnchor` checkpoint at `retention_floor`:
     - **Regenerative reset (`DESIGN-BLOCKS-06`)**: Given `points_from = min(initial_from, desired_from)`, expand `block_context_from` backwards in chunks starting from `points_from - 1 local day` across Claude, Codex, and Antigravity until EITHER (1) a `>= 5h` gap between consecutive sorted events where `e_prev.timestamp < start_of_day(points_from)`, (2) a verified `>= 5h` quiet interval `[t_earliest - 5h, t_earliest)` preceding the earliest event in the scanned prefix, or (3) the earliest available event across all configured local sources is reached.
     - **Block grouping**: Sort deduplicated priced events chronologically; open new block at `floor_to_utc_hour(event.timestamp)` whenever no block is open, `event.timestamp >= block_start + 5h`, or `event.timestamp - last_event_timestamp >= 5h`; retain every block with `block_last_event_timestamp >= start_of_day(points_from) && block_start < end_of_day(today)`.
     - **Append-preserving `BlockAnchor` (`DESIGN-BLOCKS-09`)**: Store completed blocks in `[points_from, retention_floor)`, open block state `(block_start, last_event_timestamp, cost_usd, models)` entering `retention_floor`, and per-contributor `(canonical_path, file_size, modified, scanned_through, boundary)`.
       - On steady-state polls: unchanged files (`reuse`) read 0 bytes; spanning files that pass offset + boundary validation (`resume`) whose newly decoded appended events ALL have `local_day(event.timestamp) >= retention_floor` **preserve `BlockAnchor` untouched** and advance tracked metadata without reading historical prefixes.
       - Rebuild `BlockAnchor` from regenerative reset on: backdated pre-floor appended event, truncation, replacement/in-place edit failing boundary check, file removal, newly discovered pre-floor file, deeper range expansion (`desired_from < cached_points_from`), timezone change, or restart. On monotonic midnight advance of `retention_floor` without historical mutation, advance `BlockAnchor` in memory without historical disk reads.
4. **`crates/ccusage-core/src/aggregation/rolling.rs`**:
   - `selected_period_cost(cycle, interval, snapshot_inputs, directory_selections, tz) -> Decimal` used identically by tray, `usage-snapshot`, and `GET /api/budget` (unfiltered and directory-filtered).
   - Half-open `[interval.start, interval.end)`: `start <= ts && ts < end`.
   - **Long active `customHours(n)` (`DESIGN-ROLLING-08`)**: For `start_day = local_day(interval.start) < retention_floor`, split into (1) pinned single-day active-boundary `UsageEventFileScan` slot for `start_day` (0 re-reads on unchanged old logs, tail-only on append, evicted immediately on `start_day` advance or cycle change, never populated by one-off historical queries), (2) completed interior days `(start_day, retention_floor)` reused directly from `UsageAggregationCache`, and (3) recent `[retention_floor, today]` from normal `UsageEventFileScan`.
   - **Authoritative per-partition legacy precedence (`DESIGN-ROLLING-10`)**: For every historical local `(local_day, agent, model)` partition—equally across both `[retention_floor, today]` and pinned `start_day`—select **exclusively one** authoritative input using `evaluate_legacy_partition_completeness`:
     - `Complete` (or native day) -> exact unfloored raw local events (cost-conserved to daily totals when applicable), filtered by `[interval.start, interval.end)` and active directory selections.
     - `PartialOrUnverifiable` or `Absent` -> **only** retained unmodified SQLite `session_metrics` rows for that `(local_day, agent, model)`, filtered by `[interval.start, interval.end)` and stored `row.directory` (never substituting surviving partial raw events and never double-counting by summing both).
     - `d == today` -> exact unfloored raw local events.
     - Remote SSH -> `session_metrics` filtered by `[interval.start, interval.end)`.
5. **`crates/ccusage-core/src/aggregation/snapshot_service.rs`**:
   - Orchestrates local native refresh (plan resolution -> fingerprint/generation fence -> cache load -> missing/legacy/Antigravity backfill via `UsageAggregationCache` -> `BlockBuilder` + `BlockAnchor` -> rolling-window input assembly -> `CostSnapshot`) and remote SSH refresh (via `RemoteCCUsageClient` + `UsageAggregationCache` unchanged).
   - Implements `CostSnapshot::applying(&self, state: &AppState, now: DateTime<Utc>, tz: Tz)` using the exact same `selected_period_cost` helper.
6. **`crates/ccusage-core/src/aggregation/merge.rs` & `collector.rs`**:
   - `CostSnapshotMerge`: merges eligible machine snapshots, stamps non-optional `machine`, excludes stale/error/never-collected/disabled machines from current intervals while keeping historical ranges readable, computes `DashboardScope` unavailable spans / last-hour gaps and independent `machineLatestEvents`.
   - `MachineSnapshotStore`, `MachineCollector` (single-flight per-machine async tasks, generation/revision fencing, range-load progress tracking, `MachineRuntimeReconciler` impl), and `PollingService`.

## Key Invariants and Pitfalls

- **Zero local `ccusage` calls**: Disabling all local Claude/Codex roots (with no Antigravity logs) produces an empty healthy snapshot without invoking any external executable.
- **No double-counting on partial legacy partitions**: Never union partial raw events with cached `session_metrics`; pick strictly one authoritative input per `(day, agent, model)`!
- **Spanning-file `BlockAnchor` read budget**: Verify via `ScanStats` that consecutive recent-only appends to a spanning file read ZERO historical prefix bytes!

## Test Cases (`crates/ccusage-core/tests/aggregation_tests.rs`)

- **Warm-cache cross-midnight billing block (`DESIGN-BLOCKS-02`)**: Seed yesterday covered in SQLite; priced events at `22:05 UTC` yesterday and `01:05 UTC` today; restart with empty scanner -> single point at `22:00 UTC` yesterday matching cold cache identically.
- **Continuous multi-day hourly chain (`DESIGN-BLOCKS-06`)**: UTC `$1` events every hour at `:05` from `points_from - 2d 23:05` through `points_from 01:05` -> full-history, narrow-range, expanded-range, and restarted replay ALL agree on `points_from 00:00` start and `$2` cost.
- **Spanning-file `BlockAnchor` (`DESIGN-BLOCKS-09`)**: Spanning log with events on both sides of `retention_floor`; two consecutive recent-only appends dated `today` -> `BlockAnchor` preserved, 0 historical prefix reads (`ScanStats`), each event counted once, matches cold replay; subsequent historical mutation (in-place prefix edit, truncation, removal, or pre-floor event) -> rebuilds `BlockAnchor` accurately.
- **Exact rolling custom-hour boundary (`DESIGN-ROLLING-03`)**: At `12:07 UTC` with `CustomHours(1)` (`[11:07, 12:07)`), `$1` at `11:06` + `$2` at `11:08` -> rolling total `$2`, `11:00` chart bucket `$3`; event at `11:07` included, event at `12:07` excluded.
- **Long active rolling window `CustomHours(240)` (`DESIGN-ROLLING-08`)**: At `2026-11-25 12:07 UTC`, window starts `2026-11-15 12:07` (`< retention_floor 2026-11-17`); on consecutive polls unchanged Nov 15 log has 0 bytes re-read, interior Nov 16 comes from SQLite (0 log reads), appended event counted once, boundary-day slot evicts cleanly when `start_day` advances or cycle changes.
- **Partial legacy partition rolling precedence (`DESIGN-ROLLING-10`)**: Both on a recent historical day AND on a pinned boundary day `start_day`, UTC `[22:00, next day 02:00)` with legacy cached `$6` at `22:45` (log deleted) + `$4` at `23:15` (log surviving) -> returns `$10` across restart, stored-directory filter includes only matching stored rows, event at `02:00` excluded.
- **Swift-era SQLite upgrade (`DESIGN-LEGACY-01` & `DESIGN-CACHE-07`)**: End-to-end `SnapshotService` over Swift-era DBs preserves cached-only Claude rows, adds Antigravity once, conserves `$10` on partial legacy logs across reopen and retry.
- **Multi-machine collector**: Single-flight concurrency, stale-generation fencing, stale machine excluded from current interval but readable for historical range, `machineLatestEvents` emitted even when all machines are stale.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --edition 2021 --check crates/ccusage-core/src/aggregation/*.rs crates/ccusage-core/tests/aggregation_tests.rs
cargo test --locked -p ccusage-core --test aggregation_tests
cargo clippy --locked -p ccusage-core --all-targets -- -D warnings
```

Done when all three commands exit `0` and every fixture above passes deterministically. (This serial Wave 3 join plan also validates `ccusage-core` clippy cleanliness across all joined Wave 2 modules.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `target` (added to `writePaths` and `artifactRoots`, gitignored via `/target/`) authorizes Cargo test and clippy build outputs for this serial Wave 3 join plan.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `target/`.

## Amendment 2026-11-27 (blocked-wave-scope-or-contract, round 2)

- **Workspace edition on standalone `rustfmt` gate**: Updated verification command to `rustfmt --edition 2021 --check crates/ccusage-core/src/aggregation/*.rs crates/ccusage-core/tests/aggregation_tests.rs` matching `[workspace.package] edition = "2021"`.
- **SQLite `source_day` readback compatibility**: Consume `rust-05-sqlite-stores`'s additive `UsageAggregationCache::merge_day` / coalesced `load()` / `session_metrics()` and `session_metrics_for_day(day, start, end)` contract transparently without altering `types.rs` or UTC 15-minute flooring (`design-rust-tauri-migration.md:123`).

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
