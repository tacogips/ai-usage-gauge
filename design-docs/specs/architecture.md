# ccusage-gauge Architecture

## Status

Updated for the pure Rust + Tauri v2 architecture (see `design-docs/specs/design-rust-tauri-migration.md` for full migration details).

## Product Boundary

`ccusage-gauge` is a macOS 14 or newer menu-bar and desktop dashboard application
implemented in pure Rust and Tauri v2, paired with a cross-platform headless
CLI and loopback HTTP server (`127.0.0.1`).

Local usage collection natively scans Claude Code, Codex, and Antigravity JSONL
logs incrementally by byte offset, computes exact decimal USD costs using cached
LiteLLM pricing (with daily refresh and built-in fallback), and derives 5-hour
billing blocks, daily metrics, and 15-minute session buckets with project
directory and reasoning-effort attribution locally in Rust without invoking a
local `ccusage` CLI subprocess or hard-link mirroring. Remote SSH machines
continue to invoke remote `ccusage` v20.0.17+ over SSH as their aggregate usage
source. Per-machine source-root selection follows
`design-machine-session-source-directories.md`. Static configuration and
mutable user state have separate ownership and storage locations.
Subscription rate limits for the local Claude Code, Codex, and Gemini (`agy`)
accounts are a separate, cost-independent data source defined in
`design-subscription-rate-limits.md` and `design-rust-tauri-migration.md`.

## Target Boundaries

- `crates/ccusage-core` owns configuration, state persistence, reset-window
  calculations, incremental local JSONL usage scanning, USD pricing, 5-hour
  block / daily / 15-minute session aggregation, SQLite caches and dashboard
  state stores, tolerant decoding of remote SSH `ccusage --json` responses,
  remote machine registry and collection, subscription rate-limit services,
  dashboard query services, the shared `MachineDashboardRouter`, the loopback
  HTTP server, and the loopback HTTP API client.
- `crates/ccusage-cli` (`ccusage-gauge`) exposes help/version, local diagnostic
  commands, the foreground loopback `serve` host, `dashboard` launcher, and
  typed `client` commands. It delegates all domain rules to `ccusage-core` and
  carries no GUI or WebKit dependency so headless Linux builds remain supported.
- `src-tauri` (`ccusage-gauge-app`) is a single-process Tauri v2 desktop app
  owning the macOS menu-bar tray icon (RGBA template rasterization, title,
  tooltip, live native menu, internal settings dialogs, and pure-Rust
  `SMAppService` Launch at Login) and the embedded SolidJS `"main"` dashboard
  window backed directly in-process via `dashboard_request` IPC.
- `frontend/` owns the bun-built SolidJS SPA. Its compiled `frontend/dist`
  assets are embedded in `ccusage-gauge-app` by Tauri and served from disk by
  `ccusage-gauge serve`.

Cargo workspace crate boundaries (`ccusage-core`, `ccusage-cli`, `src-tauri`)
remain the default extension points.

## Runtime Data Flow

1. Startup activates the persistent bootstrap logger, resolves the configuration
   path, and creates the default file only when it does not exist.
2. The application loads mutable state separately, using defaults if no state
   file exists.
3. For the `local` machine, background polling scans configured Claude Code,
   Codex, and Antigravity roots incrementally by byte offset (reading only
   newly appended bytes on steady-state polls), prices events with the Rust
   pricing engine, derives 5-hour billing blocks, daily metrics, and 15-minute
   session buckets, and persists completed historical days in SQLite
   `UsageAggregationCache`. No local `ccusage` CLI binary is required.
4. For enabled remote SSH machines, per-machine background pollers invoke
   remote `ccusage` (`blocks`, `daily`, `session`) over SSH using the fixed
   adapter and store completed historical days in per-machine SQLite caches.
5. `ccusage-core` applies the active reset window and delivers snapshots to the
   in-process tray presenter and `MachineDashboardRouter`.
6. The Tauri tray presenter updates its RGBA template icon, status title,
   tooltip, and live menu items only when rendered inputs change, avoiding
   flicker. Failed polls preserve the last successful snapshot while exposing a
   stale/warning state without terminating the app.
7. In `ccusage-gauge-app`, the dashboard window invokes `dashboard_request` IPC
   directly against the in-process `MachineDashboardRouter` with no TCP socket.
   In `ccusage-gauge serve`, the loopback HTTP service answers API queries from
   the same router and serves the compiled SPA.

Polling is single-flight per machine: a timer tick does not start a second
collection pass while one is active. Shutdown cancels polling and stops the
HTTP listener or desktop runtime cleanly.

## Configuration and Mutable State

### Static configuration

Path: `~/.config/ccusage-gauge/ccusage-config.json`

The application creates missing parent directories and writes sensible defaults
once. After creation, this file is read-only from the application's perspective,
including when values are invalid or unknown keys are present. This preserves
compatibility with Nix-managed configuration. Mutable menu actions never rewrite
it.

The version-one generated configuration is normative and contains these exact
defaults:

- `ccusagePath`: `null`; preserved for backward compatibility with existing
  configs. When non-null it must be an absolute path starting with `/`, and it
  is unused for local collection (which uses the native Rust JSONL scanner).
- `defaultResetTerm`: `daily`; this is the initial cycle used only when mutable
  state has no selected cycle.
- `dashboardPort`: loopback port, default `18081`.
- `dashboardAutostart`: `true`; whether tray mode (`ccusage-gauge-app`) opens
  the in-process Tauri dashboard window automatically at startup (`true`) or
  keeps it closed until requested from the tray menu (`false`). Neither mode
  binds a TCP listener.
- `pollIntervalSeconds`: `20`; polling cadence in whole seconds.
- `cacheRetentionDays`: `365`; aggregate-cache lifetime in days from creation.

Accordingly, a newly created file is equivalent to:

```json
{
  "ccusagePath": null,
  "defaultResetTerm": "daily",
  "dashboardPort": 18081,
  "dashboardAutostart": true,
  "pollIntervalSeconds": 20,
  "cacheRetentionDays": 365
}
```

Validation requires a port in `1...65535`, a positive poll interval, a positive
cache-retention day count, valid remote retry/timeout bounds, valid `#RRGGBB`
chart colors, a supported default reset term, and (when `ccusagePath` is
non-null) an absolute path starting with `/`. Because local usage collection
uses the native Rust JSONL scanner, `ccusagePath` is preserved for backward
compatibility with existing configuration files and is not required to resolve
to an installed local executable. Decode and validation errors are reported
with the config path and do not mutate the file.

### Aggregate cache

Path: `~/.cache/ccusage-gauge/aggregates-<machineId>.sqlite3`, or the equivalent
root set by `CCUSAGE_GAUGE_CACHE_HOME`. The synthetic local source uses
`aggregates-local.sqlite3`.

The cache stores completed historical daily and session aggregates. Snapshot
loading checks expiry on every regular polling pass, using `createdAt` and
`cacheRetentionDays`. An expired or corrupt cache is removed and rebuilt once.

For remote SSH machines, cache semantics remain unchanged: with a valid cache,
only uncached historical ranges and the current local day are requested from
remote `ccusage` (`blocks`, `daily`, and `session` in parallel), and results are
merged with cached history under the existing `sourceConfigurationFingerprint`
and `sourceGeneration` fence.

For the `local` machine, native Rust collection reads Claude Code, Codex, and
Antigravity JSONL logs directly and preserves compatible Swift-era SQLite caches
(`PRAGMA user_version <= 1`) without destructive reset. Because Swift appended
Antigravity in memory rather than persisting it to SQLite, legacy
`coverage_ranges` and `directory_coverage_ranges` certify Claude and Codex
history only. An additive, idempotently created local-only table
`antigravity_coverage_ranges` tracks completed-day Antigravity persistence:
uncovered historical days scan all three providers and commit all three coverage
markers atomically; legacy-covered days (covered by `coverage_ranges` and
`directory_coverage_ranges` but absent from `antigravity_coverage_ranges`) retain
cached Claude and Codex rows untouched (preserving cached-only history whose
source logs were removed) while atomically replacing any `agent == "agy"` rows
for those days and marking `antigravity_coverage_ranges` in one transaction;
only native-complete historical days (present in all three coverage tables) skip
historical aggregate persistence, while the current local day is always scanned
incrementally. Pre-upgrade `NULL` fingerprints continue to be adopted rather
than purged. Billing-block `points` are constructed independently from a known
regenerative block state—resolving `block_context_from` backwards from
`points_from = min(initial_from, desired_from)` to the nearest preceding
`>= 5h` idle gap across all local providers (or earliest available event when no
such gap exists)—and reusing an in-memory `BlockAnchor` checkpoint at
`retention_floor` plus incremental `UsageEventFileScan` (`reuse`/`resume`) on
subsequent polls. `BlockAnchor` tracks `(canonical_path, file_size, modified,
scanned_through, boundary)` for pre-`retention_floor` contributors: unchanged
files hit `reuse` with zero reads, and a spanning file that passes offset and
boundary validation (`resume`) with only `>= retention_floor` events in its
appended tail preserves `BlockAnchor` and advances its tracked metadata without
re-reading historical prefixes. Historical reconstruction runs only when inputs
before `retention_floor` can change (pre-floor appended events, truncation,
replacement, removal, failed boundary check, deeper range expansion, timezone
change, or restart), ensuring full-history, narrow-range, expanded-range, and
warm-cache restarted replays all produce identical point timestamps, costs, and
model sets (including both cross-midnight blocks and multi-day continuous hourly
chains) without repeatedly reading unchanged historical logs.

The cache uses SQLite 3 (via bundled `rusqlite`, compatible with existing
Swift-created SQLite files) with separate metadata, daily metric, session
metric, and coverage tables (preserving `PRAGMA user_version = 1` and existing
`session_metrics` timestamps unmodified). Decimal costs are stored as text to
preserve exact values, writes use a single transaction, and the legacy JSON
cache is removed when the SQLite store initializes.

Each cache file has one owning machine. Rows loaded from it are stamped with
that machine id before aggregation, even when older encoded records omitted the
machine field. Machine ids are validated slugs before they can influence a path;
callers cannot supply arbitrary path components. Remote caches exist only on the
host and nothing is persisted on a remote machine by ccusage-gauge.

The former single-local-machine path `aggregates.sqlite3` has a locked upgrade
contract. Before opening the local cache, a sole valid regular legacy database
is checkpointed, closed, permissioned to `0600`, and atomically renamed in the
same mode-`0700` directory to `aggregates-local.sqlite3`; there is no copy
fallback. If the destination already exists it is authoritative, is never
merged or overwritten, and any legacy source is retained but ignored with a
sanitized conflict warning. An invalid or unsafe legacy path is retained and
ignored while local history is rebuilt. A permission, checkpoint, race, or
rename failure is `cache_failed`, publishes no partial destination, and retries
later. Clearing a scope containing `local` serializes with collection and
atomically stages both path namespaces plus SQLite sidecars before publishing
the empty store, preventing later resurrection of legacy data.

### Mutable state

Path: `~/.local/ccusage-gauge/state.json`

The state store owns:

- optional nonnegative `budgetUSD`;
- selected aggregation period: `hourly`, `daily`, `weekly`, `monthly`, or `customHours` with a
  positive hour count;
- period baseline metadata, as defined below, which makes the selected effective
  boundary explicit and stable across restarts;
- `menuBarPieCharts`, the menu-bar pie selection. It is decoded when present
  and defaults to `[budget]`. See `design-subscription-rate-limits.md`.

State changes use an atomic replacement in the same directory. Timestamps are
encoded as ISO 8601 instants. Corrupt state is reported and must not be silently
overwritten; the UI may continue with safe in-memory defaults until the user
chooses a recovery action in a later design.

Both stores accept injected base URLs in tests. Production path resolution must
not be captured in static globals so CLI and tests can safely isolate files.

### Persistent startup and bootstrap log

The menu-bar process and local CLI runtimes establish a `ccusage-core` bootstrap
logger before loading or creating configuration, state, machine-registry,
cache, or dashboard assets. Construction is side-effect free. After command
selection, menu-bar bootstrap and only CLI commands that enter a runtime call
`activate()` before configuration parsing; help and version return without
activation and retain their no-storage behavior. Activation validates or
creates the log directory, selects any fallback, acquires the maintenance lock,
and applies retention, but does not create the active log file until the first
append. Malformed configuration JSON, registry decoding or validation failure,
executable resolution failure, cache recovery failure, listener startup
failure, and other early bootstrap failures are therefore recorded before the
existing UI or stderr error is presented.

`AppPaths` exposes `logDirectory`. Its normal production value is
`~/.local/ccusage-gauge/logs`, derived from the same state root as
`~/.local/ccusage-gauge/state.json`; `CCUSAGE_GAUGE_STATE_HOME` therefore
relocates both. If creating or appending under an explicitly overridden state
root fails, the logger makes one attempt at the default
`~/.local/ccusage-gauge/logs` location. It records only that the primary
location was unavailable, never the rejected path or underlying exception. If
both locations fail, bootstrap continues to its normal UI or stderr error path
without recursive logging.

The directory is a current-user-owned real directory with mode `0700`; active
and rotated logs are current-user-owned regular single-link files with mode
`0600`. Symlinks, hard links, non-regular files, unsafe ownership, and broader
permissions fail closed for logging and are never repaired by following or
overwriting the unsafe object.

The active file is `ccusage-gauge.jsonl`. Each line is one bounded JSON object:

```json
{"timestamp":"2026-07-16T12:00:00.000Z","severity":"error","runtime":"menuBar","phase":"configurationLoad","code":"configuration_invalid","message":"Configuration could not be loaded"}
```

`runtime` is `menuBar`, `configCheck`, `usageSnapshot`, `serve`, or `client`.
`phase` and `code` are closed application-owned identifiers. `message` is
sanitized application-owned text. Records never contain raw configuration,
stderr, environment contents, command arguments, SSH destinations or users,
identity/key values, filesystem paths, raw usage data, exception descriptions,
or request bodies. A single record is capped at 16 KiB and is encoded on one
line.

Before an append that would make the active file exceed 10 MiB, the logger
atomically renames it to
`ccusage-gauge-<UTC timestamp>-<monotonic sequence>.jsonl`, opens a new active
file, and then writes the complete record. Each successful `activate()` and
each rotation remove rotated files whose modification time is older than 72
hours. Retention never deletes the active file or unrelated directory entries.
Activation, rotation, and cleanup are serialized across tasks and processes
with an advisory lock; a lock or filesystem failure disables persistent logging
for that runtime and does not block application startup.

Clock, filesystem operations, size limit, retention duration, and destination
roots are injectable. Deterministic tests cover boundary-size rotation,
same-timestamp name collisions, retention just before/at 72 hours, unsafe file
types and permissions, fallback selection, concurrent append serialization,
and redaction of malformed JSON and SSH/process errors.

## Usage Integration and Aggregation

Local usage collection natively scans configured Claude Code, Codex, and
Antigravity JSONL logs incrementally by byte offset in `ccusage-core`, prices
events via the Rust LiteLLM + built-in fallback pricing engine, and computes
5-hour billing blocks, daily metrics, and 15-minute session buckets directly
without invoking a local `ccusage` subprocess or hard-link mirroring (see
`design-docs/specs/design-rust-tauri-migration.md`).

For remote SSH machines, the process boundary accepts an executable path,
argument list, environment, and timeout and returns stdout, stderr, and exit
status. Only fixed argument arrays are passed; no shell command interpolation
is used. A nonzero exit, timeout, invalid JSON, or unsupported payload is a
typed error suitable for UI guidance and HTTP error mapping.

Each machine owns a validated Codex/Claude source plan. For `local`, the local
`ccusage` subprocess execution and multi-source local command coordinator
instructions in `design-machine-session-source-directories.md` are explicitly
superseded by the native Rust incremental JSONL scanner running over the
resolved local `MachineSessionSourcePlan` roots (merging events by stable
identity) plus Antigravity transcripts. The schema-v3 registry contract, path
validation/normalization, source-plan resolution, cache-fingerprint
invalidation, source-generation fencing, and remote SSH fixed positional adapter
with per-agent environment isolation in
`design-machine-session-source-directories.md` remain preserved unchanged;
configured paths can select data roots but can never add executables, arguments,
environment names, or shell fragments.

Remote SSH decoders follow the ccusage 20.0.17+ `blocks --json`, `daily --json`,
and `session --json` shapes. Detailed daily loading first accepts the flag-free
ccusage 20.1+ shape, then falls back to and caches the 20.0.17
`daily --json --by-agent` shape when necessary. Agent/model breakdowns are the
source of truth for calendar-day, Monday-through-Sunday week, and calendar-month
gauge totals across supported agents, including Claude Code and Codex.
`session --json` supplies model cost and session last-activity
timestamps for hourly/custom-hour gauge totals and the dashboard's hourly graph.
They ignore unknown fields and tolerate additive schema evolution, while
requiring the time and cost fields necessary for a query. Cost is aggregated by
summing model-level cost values exactly once per record; a provided aggregate
must not be added again when model details are present. Fixtures document the
accepted schema and protect against double counting.

All internal monetary calculations use `Decimal` (`rust_decimal::Decimal`).
JSON APIs emit numeric USD values at a documented precision; UI rounding is
presentation-only.

### Project-directory dimension and filtering

The original source issue is the workflow-input request “Add per-machine
sub-directory filtering and bar-graph subdirectory split to ccusage-gauge
dashboard.” The follow-up issue is “Explicit sub-directory display names with
rename UI/API and cross-machine label uniqueness.” No GitHub issue URL,
repository/number pair, Codex-agent reference, reference repository, or Cursor
CLI behavior was supplied. This section therefore maps both workflow intakes
directly onto the existing ccusage-gauge architecture; no reference-code
divergence or Cursor adapter is required.

Project directory is optional provenance attached to usage, not a path the
application opens or executes. Claude assistant events obtain it from the
event's `cwd`; Codex token events inherit the most recent `cwd` from their
session metadata. Forward and reverse JSONL scans must produce the same
association. A missing, empty, or unusable value becomes `nil`; otherwise the
source string is retained as an opaque exact-match value. Collection never
resolves symlinks, expands home-directory syntax, or tests the directory on the
local or remote filesystem.

Local machines read candidate event lines from their resolved session-source
scan scopes. This feature does not add a remote JSONL transport, remote parser,
or general-purpose file-reading operation to the SSH adapter. Existing SSH
machines therefore continue to use remote `ccusage` aggregates without
host-side event reconciliation, and their rows have `directory == nil` unless a
future aggregate format supplies optional directory provenance directly.
`GET /api/subdirectories` returns an empty directory list for such a machine,
the sidebar renders no nested choices beneath it, and the machine's
unattributed rows remain visible. The per-machine filter contract still prevents
another machine's directory selection from excluding those rows. Transporting
raw or projected remote session content is a separate security- and
privacy-sensitive feature requiring its own explicit scope and adversarial
design review.

`TimestampedUsageEvent`, `CCUsageCostRecord`, `CCUsageMetricRecord`, and
`CCUsageSessionMetricRecord` carry this optional `directory` value through
snapshot construction. Reconciled timestamped sessions copy the event
directory while retaining the existing authoritative daily cost and token
allocation. Aggregate, coalescing, snapshot-merge, and cache identities include
directory, with `nil` as a distinct identity, so otherwise equal rows from two
projects cannot collapse.

Optional reasoning `effort` is session-level provenance parsed from Codex
`turn_context` and from the `effort` (or `perTurnEffort`) field of Claude Code
assistant lines. It follows the per-turn model context rather than the
per-session directory.

- It is carried through `TimestampedUsageEvent` and session records. Session
  merge, coalescing, and reconciliation bucket keys include it, and the
  `session_metrics` cache column stores it.
- Adding that column to the local cache resets directory-provenance coverage
  once, so the existing backfill re-derives session partitions with effort.
- A later one-time reset (SQLite `PRAGMA user_version = 1`, local cache only)
  re-derives partitions again so cached Claude history picks up effort.
- SSH-machine and `ccusage` aggregate rows, and event lines without a
  recorded effort, have `nil` (unknown) effort.

`design-dashboard-dark-flat-effort-grouping.md` holds the full contract.

For a local machine, native JSONL scanning derives `daily_metrics` and
`session_metrics` directly from Claude Code, Codex, and Antigravity logs with
project-directory and reasoning-effort provenance across every materialized
dashboard range; it is not silently limited to the recent graph window. When
upgrading a compatible Swift-era SQLite database whose source logs for a cached
historical Claude or Codex `(day, agent, model)` have since been removed, those
valid cached rows are retained rather than dropped, remaining unattributed and
following the documented `nil` filtering semantics.

SQLite daily/session cache tables carry nullable directory and effort columns,
per-host-day `directory_coverage_ranges`, and the local-only additive
`antigravity_coverage_ranges` table through idempotent additive migrations
without modifying remote cache semantics or source-generation fencing.
Pre-change cached days lacking `directory_coverage_ranges` remain
provenance-unscanned: their first directory-aware load retains authoritative
cached Claude/Codex `daily_metrics` untouched and evaluates each cached
`(agent, model)` partition against available local JSONL logs under a strict
conservation rule (because legacy `session_metrics` has no source-file
identity). A legacy `(day, agent, model)` session partition is replaced with
directory- and effort-attributed 15-minute buckets **only** when scanned local
events are complete—their summed integer token counts exactly equal the
authoritative cached `daily_metrics` token totals (with at least one non-zero
token count, or both cached and raw costs zero)—and the replacement buckets
conserve the authoritative `daily_metrics` cost and token totals exactly. When
surviving logs are partial/unverifiable (for example, one of two logs survives
and reconstructs less than the cached daily/session total) or absent, the
conservative fallback retains the existing valid cached `session_metrics` rows
untouched without partial overwrite, duplication, or assigning missing usage to
surviving directories. In the same transaction, missing Antigravity
(`agent == "agy"`) rows are atomically replaced and both
`directory_coverage_ranges` and `antigravity_coverage_ranges` are marked.
Swift-era days already present in both `coverage_ranges` and
`directory_coverage_ranges` but absent from `antigravity_coverage_ranges` retain
all cached Claude and Codex rows untouched while atomically replacing any
`agent == "agy"` rows for that day and marking `antigravity_coverage_ranges` in
one `BEGIN IMMEDIATE ... COMMIT` transaction.

Failure or interruption rolls back both the row replacements and coverage
markers, retains the previously published snapshot, and leaves the day eligible
for retry. A retry recomputes and replaces the affected partition cleanly (for
reconstructible provenance partitions or `agent == "agy"`); it never merges
additively into duplicate legacy or Antigravity copies. Snapshot publication
occurs only after commit, so no reader can observe partial or duplicated rows.
Native-complete historical days (covered by `coverage_ranges`,
`directory_coverage_ranges`, and `antigravity_coverage_ranges`) are reused
without repeated historical aggregate writes; the current host day is always
recomputed from incremental scans on refresh. Previously encoded records still
decode a missing directory as `nil`. Unfiltered totals remain identical across
initial backfill, reopen, and any failed or retried backfill.

Remote SSH `ccusage` daily/session rows and retained legacy unattributed rows
that do not identify a project remain unattributed. An unfiltered query that
does not request a directory breakdown continues to use `dashboardMetrics`. A
daily query with one or more directory filters, or with the optional
directory-breakdown request enabled, instead derives its rows by grouping
`dashboardSessions` by host calendar day, machine, agent, model, and directory.
Because local daily metrics and 15-minute session buckets are derived from the
same priced events (and legacy backfill preserves authoritative daily totals),
directory-resolved rows, including their `nil` group, have the same unfiltered
totals as the aggregate path. Unattributed session rows remain visible when a
machine has no active directory selection and are excluded when that machine has
one or more selected directories.

The read API adds an optional, repeatable query item
`directory=<machine-id>:<full-directory>`. The complete value is percent
encoded, and the server separates the validated machine id at the first colon.
No `directory` item means the existing unfiltered behavior. One or more items
for a selected machine form an exact-match OR set for that machine; filters for
different machines never intersect or affect one another. A well-formed filter
for a known machine outside the current `machine` selection is ignored, which
makes an unchecked machine neutral even if a stale client retains its prior
values. Malformed entries and unknown machine ids return a non-sensitive `400`
or `404` response using existing selection conventions. Directory values must
not appear in persistent logs or error messages.

`/api/metrics`, `/api/cost-series`, and `/api/budget` accept the directory
items. Their rows expose optional `directory`; filtered totals and the budget's
spent-derived values are recomputed from the filtered rows. In addition,
`/api/cost-series` accepts the optional single query item
`directoryBreakdown=true`. Omission or `false` preserves the existing response
path and grouping when there are no directory filters. `true` requests
directory-resolved rows independently of filtering, including for daily
granularity; duplicate values or values other than `true` and `false` return
`400`. A directory filter also selects the directory-resolved path regardless
of this flag. Existing clients that omit both additions receive unchanged rows,
totals, and grouping.

The additive `GET /api/subdirectories?machine=<id>` route accepts the same
machine-selection contract as the snapshot-backed dashboard data routes: omit
`machine` or pass exactly `machine=all` for all enabled machines, or repeat
`machine=<id>` for one or more distinct canonical ids. The `all` sentinel
cannot be mixed with ids, and duplicate, empty, or non-canonical values return
`400`. The route returns stable, full-string-sorted values grouped by machine.
`names` is an optional map from full directory value to explicit display name:

```json
{"machines":[{"machine":"local","directories":["/Users/example/project"],"names":{"/Users/example/project":"Billing"}}]}
```

The list is derived from the selected machines' retained, provenance-scanned
snapshot coverage, excludes `nil`, and exposes the full value only so the
loopback SPA can submit exact filters. A historical range expansion refreshes
the list after the expanded snapshot is published, so newly observed
directories become selectable. A `names` entry is returned only when its
directory is present in the same machine's `directories` array; stored names
for paths that are not currently discovered are retained but not disclosed.
The field is omitted when a machine has no applicable explicit names, so
clients that only decode `directories` remain compatible. The UI never
displays the full path by default. `DashboardCostRow`, metric response rows,
and their frontend counterparts encode `directory` only when present,
preserving decoding compatibility for pre-change snapshots, caches, saved API
fixtures, and clients.

Explicit names are dashboard-host metadata, not usage provenance and not
per-browser UI state. They are stored in the existing
`~/.cache/ccusage-gauge/dashboard-state.sqlite3` database, or the equivalent
file under `CCUSAGE_GAUGE_CACHE_HOME`, in a separate idempotently created table
keyed by canonical machine id and opaque full directory string, with the
normalized non-empty display name and update time.
The table is independent of the singleton `DashboardUIState` value, so an API
rename is visible to every browser and survives reloads and application
restarts. No name is sent to or persisted on a remote machine. Upsert and clear
operations are serialized by the store; the last successfully committed
request for a key wins. A stored name remains available if a directory
temporarily disappears and applies again if the exact machine/path key is
rediscovered.
Deleting a configured machine is different from temporary directory
disappearance: the registry transaction first persists a dashboard-name
deletion marker in a prepared phase, which immediately excludes that machine's
names from reads. After registry persistence succeeds, the transaction advances
the marker to a committed phase before publishing the runtime transition, then
immediately attempts to purge the rows while retaining the marker as a durable
barrier. Before restoring the previous registry after a failed runtime
transition, the transaction advances the marker to a rollback phase. After the
registry restoration is durable it removes that marker, so the unchanged names
become visible again. If post-commit purge fails, the committed machine deletion
remains successful, the marker continues to hide the names, and the mutation
owner records the machine id as pending metadata cleanup. It retries cleanup
with bounded same-process
backoff and on the next machine-catalog read; `GET /api/machines` exposes any
remaining ids through optional `metadataCleanupPendingMachineIds` so the SPA can
show a reconciliation warning. A rolled-back deletion removes the marker and
restores the unchanged names. Creating a machine under a previously used canonical id
atomically removes any retained rows and the marker before the registry commit,
so a replacement machine cannot inherit the deleted identity's labels. These
membership transitions apply equally to API mutations, bulk replacement, and
external registry reload.
Before either production dashboard router becomes available, startup
transactionally reconciles this metadata with the persisted registry. A
prepared marker for a machine id still present in the registry is removed so an
interrupted pre-persistence deletion cannot keep valid names hidden after
restart. A rollback marker for a machine id still present is also removed
without deleting names, covering termination or marker-cleanup failure after
the previous registry was restored. A committed marker for an id that is
already present again identifies machine-id reuse after a durable deletion:
startup purges the old names before removing the marker. Metadata for ids absent
from the registry is promoted to a committed marker and purged regardless of
its prior phase, completing cleanup that may have been interrupted after the
registry commit or before a rollback completed. A
reconciliation storage failure fails dashboard startup rather than serving
ambiguous or stale names.

Both local `HTTPService` mode and `MachineDashboardRouter` expose
`PUT /api/subdirectories/name` with this request shape:

```json
{"machine":"local","directory":"/Users/example/project","name":"Billing"}
```

`machine` must be a canonical id and must exist in the local or machine
registry scope served by that router. The local-only surface accepts only
`local`. `directory` is an opaque, non-empty string and is not resolved,
opened, normalized, or required to be present in the current snapshot. The
`name` member is required but its value may be `null`; a null, empty, or
whitespace-only value clears the row. Non-empty names are trimmed at their
edges, must contain no control characters, and are limited to 200 Unicode code
points. A successful set returns
`{"status":"ok","machine":"local","directory":"/Users/example/project","name":"Billing"}`;
a successful clear returns the same object with `name:null`.
The name-store write transaction rejects a machine id with an active deletion
marker. This closes a stale-authorization race with concurrent machine deletion;
the route returns the same sanitized `404 machine_not_found` response used when
the machine has already left the served registry.

Malformed JSON or invalid machine, directory, or name values return `400` with
`invalid_directory_name`; an unknown canonical machine returns `404` with
`machine_not_found`; persistence failure returns `503` with
`directory_name_unavailable`. Responses and persistent logs never include
database details or untrusted path/name values in error messages. Both the
local-only `HTTPService` surface and `MachineDashboardRouter` apply the same
common loopback-authority, exact same-origin/fetch-metadata, and
`X-CCUsage-Gauge-Mutation: 1` gate. That gate runs before request-body decoding,
machine or directory validation, and store access, and a rejected request makes
no observable state change. `OPTIONS` is rejected under the same control-route
policy. The SPA sends the mutation header; non-browser automation may omit
browser origin and fetch metadata but must send the header. A failed write does
not update the effective name. A name-store read failure fails the subdirectory
request with `503` instead of silently presenting derived labels as if explicit
names had been cleared.

The SPA owns selected directories as a map keyed by machine id. Only a checked
machine renders its nested directory checkboxes. Unchecking it removes that
machine's active directory set and omits its directory query items; rechecking
therefore starts unfiltered. Selecting no nested checkbox also means unfiltered
for that machine. Machine, model, and agent filters continue to compose as
logical AND dimensions.

Directory labels are deterministic presentation values allocated in one pass
across every machine in the dashboard's subdirectory catalog. The SPA fetches
that catalog for all enabled machines independently of the active machine
filter, so toggling a machine does not renumber another machine's labels.
Retained snapshots for enabled stale machines remain inventory sources even
while their usage is excluded from current totals. Machine create, edit,
enable, disable, and removal operations refresh the catalog before presenting
the resulting global allocation.
Flatten the machine/path entries and sort first by machine id and then by full
directory string. For each entry, use its explicit name as the base when
present. Otherwise remove trailing path separators, take the deepest non-empty
component (using `/` as the label for the root), then take its first 10 Unicode
code points.

Use the base label when it is globally unused; otherwise append the smallest
`-2`, `-3`, and so on that produces a label not already allocated anywhere in
the catalog. The uniqueness check covers collisions between two derived names,
two explicit names, an explicit and a derived name, and a generated suffix
colliding with another entry's base. Explicit names are not truncated; any
suffix is appended to the complete explicit name. Full directory strings
paired with machine ids remain the stable selection and series identities, so
renaming changes presentation without changing filters, data grouping, or
color identity.

Each sidebar directory row provides an inline rename action. Starting an edit
copies the current explicit name, or an empty value when only a derived name
exists. Enter or blur submits the `PUT`; Escape cancels; submitting a blank
value clears the name and restores the derived fallback. After a successful
response the SPA updates its names catalog, reruns global label allocation, and
immediately updates both the sidebar and chart legend. A failed response leaves
the last confirmed label in place, keeps the edit recoverable, and shows a
non-sensitive inline error. A transport timeout, truncated response, or lost
response is not authoritative because the idempotent `PUT` may already have
committed. For that ambiguous result, the SPA refetches the catalog and compares
the exact normalized machine/path name: a match confirms and closes the edit;
a mismatch or failed refetch retains recoverable input and reports that the
outcome is unconfirmed rather than falsely declaring failure. A subsequent
subdirectory fetch remains the source of truth and proves persistence after
reload.

The graph's stack control adds `subdirectory` beside the existing `model` and
`machine` values (`modelEffort` is added later by
`design-dashboard-dark-flat-effort-grouping.md`). It defaults to the existing non-subdirectory state, so
subdirectory splitting is disabled until explicitly selected. Old persisted
`model` and `machine` values remain valid; missing or unknown saved values fall
back to `model`. The frontend sends `directoryBreakdown=true` to
`/api/cost-series` only while `subdirectory` stacking is selected; it omits the
item for model or machine stacking. Active directory filters remain separate
query items and continue to request directory-resolved rows even while another
stack mode is selected. When subdirectory stacking is off and no directory
filter is active, bucket totals and visual series match current behavior. When
it is on, the series identity combines machine and full directory so equal
names on different machines never merge. Its displayed name is the same
globally allocated explicit-or-derived label used by the sidebar; machine-name
prefixing is no longer needed to disambiguate discovered directories.
Unattributed rows use the existing distinct, machine-aware `No directory`
presentation unless an active directory filter has excluded them.
Subdirectory series use their own deterministic color namespace keyed by
stable machine/path identity, so a rename does not change series color and does
not perturb existing model or machine color assignments.

Compatibility and behavior are locked by tests for event decoding with and
without `cwd`, Codex session-metadata inheritance in both scan directions,
historical-range provenance loading, pre-change cache provenance backfill,
cache/snapshot decoding without directory, merge-key separation, machine-local
filtering, unattributed-row inclusion and exclusion, deterministic globally
unique label collision suffixes (including cross-machine derived, explicit,
mixed, and suffix-versus-base collisions), persistent name set/clear/reopen,
rename validation and both routing surfaces, mutation-gate parity and rejection
before decoding or persistence, optional-name response decoding,
sidebar and chart label refresh without identity/color changes, lost-response
rename reconciliation, rollback-phase restart recovery, nested-filter
visibility, historical-list refresh, machine-uncheck reset, query encoding,
daily directory-breakdown requests with and without active filters,
omitted/false breakdown compatibility, persisted stack-value fallback, atomic
backfill rollback/retry, no legacy-and-attributed session duplication, and
unchanged totals before and after successful or failed backfill. Targeted
checks precede the full implementation gate:

```text
cargo test -p ccusage-core directory
cd frontend && bun test machineScope
cd frontend && bun test api
cargo build --workspace
cargo test --workspace
cd frontend && bun test
```

## Aggregation Period and Budget Rules

### Persisted reset baseline contract

The baseline is a persisted cache of the selected period boundary used to include or
exclude `ccusage` records. It is not a monetary balance, does not copy usage
cost, and must never be added to or subtracted from `ccusage` totals. Version one
stores this object in `state.json`:

- `scheduledBoundaryAt`: scheduled boundary calculated for the selected cycle;
- `activeBoundaryAt`: the scheduled boundary used for aggregation;
- `cycle`: a copy of the cycle used for the calculation, including the positive
  hour value for `customHours`;
- `calendarIdentifier` and `timeZoneIdentifier`: environment used for calendar
  boundaries;
- `computedAt`: evaluation instant, used for diagnostics and expiry checks.

All baseline timestamps are ISO 8601 instants. The baseline is valid only when
its cycle, calendar, time zone, active boundary, and scheduled boundary match
the current evaluation context. `computedAt` never changes the cost result.

Startup loads state, chooses the persisted cycle or the configured
`defaultResetTerm`, and validates the baseline. A missing or stale baseline is
recomputed before presenting cost and atomically persisted with the state.
Advancing into a new scheduled window similarly recomputes and persists it.
Corrupt state remains an error under the state-store policy and is not repaired
silently.

Changing the aggregation period recomputes all baseline fields and atomically
persists the selection and baseline. There is no manual-reset override.

Every menu-bar, CLI, and dashboard cost query uses `baseline.activeBoundaryAt`
as the lower bound after baseline validation. There is no parallel boundary
calculation at presentation or API layers. This persisted derivation ensures a
restart, cycle change, and subsequent poll all apply the same boundary contract
while the native local usage engine (for `local`) and remote `ccusage --json`
(for SSH machines) remain the sole monetary sources.

### Boundary calculation and aggregation

Calendar boundaries use the user's current time zone:

- `hourly`: the current local clock hour, from `HH:00:00` through `HH:59:59`;
- `daily`: the current local day, from `00:00:00` through `23:59:59`;
- `weekly`: Monday `00:00:00` through Sunday `23:59:59`;
- `monthly`: start of the current local month;
- `customHours(n)`: `n` hours before the evaluation instant.

Usage outside the selected interval is excluded. Tests use fixed clocks,
calendars, and time zones and cover exact boundaries, Monday-based weeks,
daylight-saving-relevant calendar transitions, cycle changes, and restart persistence.

Cost is the sum of records in the selected half-open interval
`[interval.start, interval.end)` where `interval.start == baseline.activeBoundaryAt`
(inclusive start, exclusive end; an event at `start` is included and an event at
`end` is excluded). Calendar-day, week, and month totals use exact daily
agent/model rows. For `hourly` and rolling `customHours(n)` periods, 15-minute
chart buckets (`dashboardSessions` / `session_metrics`) remain distinct from
exact rolling-window inputs: local rolling totals sum unfloored priced local
events satisfying `interval.start <= event.timestamp < interval.end` within the
retained in-memory scan window `[retention_floor, today]`. When an active
`customHours(n)` window begins before `retention_floor` (e.g., `customHours(240)`),
completed interior days strictly between `start_day = local_day(interval.start)`
and `retention_floor` are reused directly from `UsageAggregationCache` (daily or
session aggregates, with zero raw log reads), while `start_day` is pinned in a
bounded single-day active-boundary `UsageEventFileScan` slot so unchanged old
logs hit `reuse` (zero re-reads) and appended logs hit `resume` (tail-only) on
consecutive polls; advancing `start_day` or changing cycles immediately evicts
the stale boundary slot, and one-off historical dashboard range scans never
populate it. For every historical local `(day, agent, model)` partition—equally
across both the recent window `[retention_floor, today]` and the pinned
boundary day `start_day`—rolling totals select exactly one authoritative input:
complete native or reconstructible legacy partitions (exact integer token match
with cached `daily_metrics`) use exact unfloored raw event timestamps, whereas
partial, unverifiable, or absent-log legacy partitions use **only** their
retained SQLite `session_metrics` rows (never rewriting them, never substituting
partial raw scans, and never adding raw events on top of cached rows), filtered
by the same half-open `interval.start <= row.timestamp < interval.end` predicate
and stored `row.directory` when directory filters are active. Remote SSH
`session_metrics` rows continue using the same half-open timestamp filter. The
menu-bar tray, CLI `usage-snapshot`, and `GET /api/budget` (unfiltered and
directory-filtered) all apply this identical half-open window contract. Baseline metadata contributes no cost. With a
positive budget, spent is the nonnegative cost, remaining is
`max(budget - spent, 0)`, and the visual fraction is capped at 100% while the
raw over-budget amount remains available. Without a budget, the UI shows an unset
state rather than inventing a denominator.

## Menu-Bar Behavior

Launching `ccusage-gauge-app` in tray mode runs as a macOS menu-bar accessory
app sharing one in-process `MachineCollector`, `MachineDashboardRouter`, and
`RateLimitUsageService` with no TCP listener. Startup honors
`dashboardAutostart` from `ccusage-config.json`: when `true` (default), it opens
the in-process `"main"` Tauri dashboard window automatically; when `false`, it
keeps the window closed until opened from the tray menu. Closing the dashboard
window hides/closes it and returns to accessory mode without stopping polling or
quitting. Launching `ccusage-gauge dashboard` resolves the GUI executable via
`CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` (validated without sibling fallback when set)
or sibling `ccusage-gauge-app` lookup when unset, and runs
`ccusage-gauge-app --dashboard-only` to open a single tray-free in-process IPC
window with no TCP listener that exits cleanly when the window closes.

The Tauri v2 tray icon renders a combined 2x Retina RGBA template image via
`TrayIcon::set_icon_with_as_template` (area-proportional fill provider logos for
Claude, Codex, and Gemini, the budget pie icon with warning triangle on failure,
and optional inline ratio percentages), accompanied by the formatted USD cost
via `TrayIcon::set_title` (when `menuBarShowCost` is enabled). Updates are
skipped when display inputs are unchanged to prevent flicker. Its native menu
exposes:

- live budget usage summary and selected rate-limit rows updated via
  `MenuItem::set_text`;
- `Usage icons` and `Ratio text` toggles plus a `Menu bar display…` settings
  dialog persisted to `state.json`;
- `Set budget…` (USD budget and contributing machines), `Aggregation period`
  (`Hourly`, `Daily`, `Weekly`, `Monthly`, `Custom hours…`), and
  `Refresh interval` controls persisted to `state.json`;
- a `Settings` submenu with `Launch at Login` backed on macOS by pure-Rust
  `SMAppService.mainAppService` FFI;
- a warning status icon and `Error Details` submenu when collection or
  configuration fails, including the config path and a `Retry validation`
  action;
- in-process dashboard window open/close toggle (`Open dashboard`,
  `Open dashboard window` / `Close dashboard`), `Refresh`, and `Quit`.

See `design-docs/specs/design-rust-tauri-migration.md` for full tray rendering
and lifecycle details.

## Dashboard Service

The service listens only on IPv4 loopback `127.0.0.1`, never `0.0.0.0`, IPv6
any-address, or a network interface selected from configuration. Startup fails
clearly if the configured port cannot be bound. Stop closes the listener and
active resources deterministically; repeated start and stop actions are safe.

Version-one routes are:

- `GET /` and SPA asset paths: compiled frontend files, with SPA fallback only
  for non-API navigation paths;
- `GET /api/recent`: model-summed 5-hour billing-block cost series (`points`),
  computed independently of SQLite aggregate coverage with cold/warm-cache
  parity including cross-midnight active blocks;
- `GET /api/day?date=YYYY-MM-DD`: time-of-day billing-block breakdown (`points`)
  for the selected local date and its total, preserving cold/warm-cache parity;
- `GET /api/period?range=today|yesterday|week|month`: billing-block total and
  series (`points`) for a predefined local-calendar period with cold/warm-cache
  parity;
- `GET /api/period?range=custom&start=YYYY-MM-DD&end=YYYY-MM-DD`: billing-block
  total and series (`points`) for inclusive whole local days, rejecting missing,
  malformed, or reverse-ordered bounds, with cold/warm-cache parity;
- `GET /api/metrics?range=all|today|yesterday|week|month`: exact daily
  agent/model cost and token breakdowns for the selected period;
- `GET /api/metrics?range=custom&start=YYYY-MM-DD&end=YYYY-MM-DD`: the same
  detailed metrics for inclusive whole local days;
- `GET /api/cost-series?granularity=15min|hourly|daily&range=...`: filtered
  graph source rows; 15-minute and hourly views use 15-minute chart buckets
  (`dashboardSessions` / `session_metrics`) while daily uses exact daily
  agent/model breakdowns (or directory/effort-resolved session rollups);
- `GET /api/budget`: budget, selected-period cost (applying the same exact
  half-open `[activeBoundaryAt, end)` rolling contract as the tray and
  `usage-snapshot`, both unfiltered and with `directory` filters), remaining
  amount, aggregation period, active boundary, and the effective menu-bar
  refresh interval.
- `GET /api/rate-limits`: derived 5h and weekly subscription windows for the
  local Claude Code and Codex accounts. Upstream calls are throttled to one
  per provider every 60 seconds. The route is served on both routing surfaces,
  ignores machine selection, and has no snapshot dependency. See
  `design-subscription-rate-limits.md`.

The dashboard defaults to a rolling last-12-hours hourly query. It requests only
the selected period, so Today, Yesterday, This Week, This Month, and custom
historical data are loaded lazily rather than through an all-history catalog.
It automatically refetches the selected-period metrics, cost series, and budget
at the effective menu-bar refresh interval. A changed menu-bar interval is
returned by `/api/budget` and reschedules dashboard polling without requiring a
page reload. Background refreshes retain the last completed dashboard state;
the loading screen is reserved for requests that do not yet have a completed
value, such as the initial page load. A non-blocking `Updating…` status remains
visible while a background refresh is in progress.

User-initiated range changes use a blocking transition: the previous graph is
cleared and the initial loading state remains until both selected-period metrics
and cost-series resources finish. Timer and manual refreshes continue to use the
non-blocking background state.

Graph granularity changes use the same blocking transition and complete after
the replacement cost-series resource finishes loading.

The router coalesces concurrent API snapshot reads through an actor-isolated
in-flight task and briefly reuses the completed result. A frontend refresh can
therefore request metrics, cost series, and budget concurrently while
`ccusage-core` runs only one snapshot load instead of one full collection pass
per endpoint.

The server starts snapshot prewarming as soon as its listener starts. Normal
read endpoints reuse that completed snapshot for up to 60 seconds, making range
changes a local filter operation. `GET /api/refresh` forces one coalesced fresh
snapshot; the frontend waits for it before refetching the three visible
resources, which then resolve from the refreshed cache.

Successful responses use JSON and stable field names. Bad parameters return
`400`; missing routes return `404`; `ccusage` or internal query failures return
`503` or `500` with a machine-readable error code and non-sensitive message.
No route accepts arbitrary filesystem paths or executes arbitrary commands. The
explicit state-changing HTTP control surface comprises machine-registry create,
replace, patch, and delete, `GET /api/refresh`, `DELETE /api/cache`, and
`PUT /api/subdirectories/name` on both the local-only and machine-scoped
dashboard routing surfaces.
Read-through historical queries may populate their selected machine caches but
cannot change configuration or delete retained data. Budget and
aggregation-period mutations remain menu-bar actions. Every explicit control
mutation uses the common loopback authority, exact same-origin/fetch-metadata,
and `X-CCUsage-Gauge-Mutation: 1` gate defined by the remote-machine design.

Static assets for `ccusage-gauge serve` are resolved in this order: an explicit
`--assets` override, adjacent Formula/Linux `share/ccusage-gauge/web`, then
macOS bundle `Contents/Resources/Web`. `ccusage-gauge-app` embeds compiled
`frontend/dist` assets directly via Tauri. Resolution is read-only and failure
produces a `503 assets_missing` diagnostic response rather than exposing a
directory listing. Formula and Cask build validation must exercise their
respective layouts.

## Remote Machine Collection

The dedicated behavior and security design is
`design-docs/specs/design-remote-machine-collection.md`. `serve` loads
`~/.config/ccusage-gauge/machines.json`, always adds the reserved synthetic
`local` descriptor, and creates one independently cancellable poller per enabled
machine. Local collection uses the native Rust incremental JSONL scanner and
pricing engine. SSH collection
executes the configured remote ccusage binary through a direct endpoint or the
dedicated design's structured SSH proxy adapter and reuses the existing JSON
decoder, but does not read host event logs or install a remote daemon. An
already-open local forward is a direct endpoint.

The collection boundary is provider-neutral. Direct SSH, `ProxyJump`,
`ProxyCommand`, local forwarding, and equivalent operator-managed tunnels share
one transport, status, diagnostic, action, and API contract. GCE and IAP may
appear only as deployment examples and never select code paths, fields, routes,
classifiers, remediation, or UI labels.

Proxy behavior is isolated behind an optional closed adapter on the SSH
descriptor. `jump` accepts validated structured hop metadata; `command` accepts
only an absolute owner-safe stdio-adapter executable. The application supplies
the validated target host and port through one fixed invocation. Raw `-J`,
`ProxyJump`, `ProxyCommand`, adapter arguments, environment/configuration
values, SSH configuration files, and shell fragments are never accepted.
Direct endpoints, local forwards, jump hops, and command adapters all preserve
target host-key verification; jump hops additionally enforce their own verified
host identity. No adapter accepts inline credential contents or exposes raw
adapter output.

The registry stores connection configuration only and is atomically written
with mode `0600` inside a mode-`0700`, current-user-owned directory. Registry
CRUD is serialized with affected collector reconciliation: the owner publishes
the new immutable revision only after durable staging and runtime replacement
agree. Runtime failure rolls disk and collector state back; failed compensation
stops the affected generation and rejects later mutations until restart
recovery reconciles one complete persisted revision. Registry load is
fail-closed: only an absent file means an empty SSH registry; unsafe
ownership/type/permissions, malformed JSON, invalid descriptors, or an unsafe
persistence path fail service startup without quarantine or local-only fallback.
Recovery requires an offline correction or intentional removal of the file.
The current persisted representation is the session-source design's closed
version-3 `schemaVersion`, `localSessionSources`, and `machines` envelope.
Exact version-1 and version-2 representations are accepted only as migration
sources and are atomically rewritten to version 3 before registry publication
or poller startup. Migration failure preserves the source bytes and fails
startup. Persisted `machines` remain SSH-only; local source settings live in
the dedicated top-level object. Every version rejects unknown or duplicate
fields; there is no implicit unversioned or unknown-field compatibility.
Machine ids are immutable, unique safe slugs; `local` cannot be disabled,
replaced, or deleted. SSH host, user, port, identity path reference, extra
options, and remote executable are validated before use. Process arguments are
arrays rather than local-shell strings. Ambient SSH config is disabled, remote
tokens are POSIX-quoted, options use the closed allowlist, and values capable of
changing config, hooks, environment, forwarding, or the remote-command boundary
are rejected as specified in the dedicated design. Raw proxy options are also
rejected; only the separately validated structured proxy adapter may select jump
or command behavior.

An actor-owned snapshot store retains the latest successful snapshot and
sanitized health status independently for each machine. A failed refresh does
not erase the last successful snapshot. One mutation owner serializes candidate
validation, synchronized atomic persistence, immutable revision publication,
old-generation cancellation, and affected-poller replacement before replying.
Poll publications are revision/generation fenced; unaffected pollers continue
running.

Every snapshot-backed dashboard data route defaults to all enabled machines
when `machine` is omitted. A caller may instead pass exactly `machine=all` or
repeat `machine=<id>` to select one or more distinct canonical ids. The `all`
sentinel cannot be mixed with concrete ids, and duplicate, empty, or
non-canonical values return `400`. Unknown ids return `404`; disabled ids return
`409`; enabled ids without a snapshot return `503`. Health and control routes
retain their separately documented single-id-or-`all` contracts. Retained stale
snapshots remain readable only for explicitly historical intervals. Before any
interval reaching the current host day is aggregated, selection excludes
machines whose derived state is stale, error, never-collected, or disabled.
Their retained rows therefore cannot enter current series, totals, budget
values, or summary cards.

The all-machines view merges only snapshots eligible for the requested interval
and returns partial `200` results when at least one machine remains. It stamps
every block/timeline, daily, and session record with non-optional source
provenance and emits `machine` on every recent/day/period series point, metric
row, and cost-series row. Aggregation keys preserve machine identity, and totals
and the single host-budget summary are recomputed from eligible rows instead of
summing precomputed values. Query scope identifies every excluded machine, its
concrete unavailable-since time and reason, and the intersection of its data gap
with the last hour. Host calendar and reset rules define aggregate boundaries,
while the oldest included generation time describes aggregate freshness.

`GET /api/machines` provides registry listing and SSH CRUD, while
`GET /api/machine-status` reports healthy, stale, disabled, never-collected, and
structured sanitized SSH proxy/tunnel failure details. `/api/cost-series`
carries each
selected machine's latest-event marker in both successful responses and
recognized data-availability error envelopes. Marker derivation is independent
of current-row eligibility, so an all-stale or concrete-stale selection still
reports marker metadata without allowing retained stale rows into totals.
Guarded per-machine test-connection and targeted refresh actions reload
validated registry configuration and make edits usable without a process
restart. Registry mutations, connection tests, manual refreshes, and cache
deletion share the dedicated design's loopback authority, same-origin,
fetch-metadata, and mutation-header policy; rejected requests change no state
and receive no CORS authorization.

Diagnostic classification is closed and ordered across host-key,
authentication, proxy/tunnel reachability, timeout, remote-command,
invalid-response, and cache failures. Only failures outside those typed
boundaries use the sanitized `internal_error` fallback; raw stderr and exception
text never cross into APIs, UI, CLI output, or persistent logs.

Cache clear is atomic for each selected machine and partial across `all`.
Complete, mixed, and zero-success results use stable `200`, `207`, and `500`
responses with per-item `cache_failed`; `all` includes disabled descriptors. A
clean rollback retains and resumes the old store, while an unrecoverable
interruption retains stale data and stops only that machine's poller. Startup
must resolve an interrupted clear to either the complete prior state or the
complete empty state before opening the cache. Concrete durability and recovery
mechanisms belong to the implementation plan.

## Frontend Contract

The SolidJS SPA is built by bun and produces static assets. A left sidebar
selects All machines or one registered machine and filters exact `ccusage`
daily breakdown rows by model and agent. The selected scope is visible, and
rows expose machine attribution in all-machines scope. A Machines screen manages
SSH descriptors, their closed direct/jump/command adapter fields, enablement,
and collection health without displaying secret contents or accepting raw
proxy commands. It provides guarded Test connection and Refresh controls and retains
each sanitized action result until the next edit or action. Stale or unavailable
machines have a persistent high-contrast state that includes last success,
failure reason, unavailable-since time, and the last-hour data gap. Summary
cards identify excluded machines and consume only server-selected eligible
rows. The top-right
aggregation control provides Today, Yesterday, This week, This month, and a
Custom choice that reveals From/To date calendar controls. The graph always
shows cost and provides Hourly and Daily aggregation controls, defaulting to
Hourly. Its sub-daily view renders each selected machine's latest-event marker
and unavailable spans from successful or recognized data-availability
responses without treating stale retained history as current usage.
The selected period and filters drive all totals, the green-bar cost series, and
daily detail;
mixed-model block costs are never used for dashboard model filtering. The SPA
reads only the same-origin JSON API, treats API failures and empty series as
first-class UI states, and does not invoke `ccusage` or access local files.

`design-dashboard-dark-flat-effort-grouping.md` defines the following:

- the dark-only flat theme, with no border-radius, shadows, or gradients;
- square buttons whose pressed state is signalled by `aria-pressed`, which
  also fixes the range-preset active state;
- persisted `sidebarCollapsed` and `headerCollapsed` fold state;
- the opt-in `modelEffort` stack mode, backed by
  `/api/cost-series?effortBreakdown=true`;
- vendor-family, collision-free model color allocation with effort shades.

The model-only stack remains the default, and responses without
`effortBreakdown` are unchanged.

`design-subscription-rate-limits.md` defines the rate-limit panel and the
persisted `DashboardUIState.pieCharts` multi-select of pie charts. The
selection defaults to the budget pie only.

## Security and Privacy Boundaries

- The server is loopback-only. Registry create/replace/patch/delete, manual
  refresh, and cache deletion are the explicit state-changing HTTP controls and
  all use one fail-closed same-origin plus mutation-header gate.
- `ccusage` arguments are fixed and never composed through a local shell.
- SSH destinations, ports, option arguments, identity path references, and the
  remote executable are validated; no inline private key is accepted or logged.
- API errors, logs, and UI guidance exclude environment values, raw usage
  payloads, and unrelated local paths.
- Registry safety is mandatory rather than best-effort: unsafe ownership, file
  type, links, permissions, JSON, descriptors, or persistence paths fail startup.
  Other configuration and state files use user-only permissions when created.
- Claude Code and Codex OAuth credentials are read-only and held in memory
  for a single fetch. They are never refreshed, logged, persisted, or
  returned. `/api/rate-limits` returns only derived numbers, as specified in
  `design-subscription-rate-limits.md`.
- The user-provided HEIC image is a visual reference only and must not be
  copied into the repository, build resources, release archives, or Git.

## Rollout and Verification Constraints

Implementation proceeds in dependency order: documentation; `ccusage-core`
contracts and unit tests; `ccusage-cli` commands and tests; `src-tauri` tray
and dashboard app; packaging, smoke, and CI scripts; Swift artifact removal;
then full verification and documentation synchronization. Narrow crate tests
and `cargo clippy` run after each milestone. The final gate includes
`mise run frontend:check`, `mise run frontend:test`, `mise run frontend:build`,
`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo build --workspace`, CLI/dashboard/asset smoke checks, and Homebrew
Formula/Cask `--dry-run` checks.

Built frontend assets must have an explicit packaged-resource location for both
Homebrew Formula (`share/ccusage-gauge/web`) and Cask (`Contents/Resources/Web`
plus Tauri embedded `frontend/dist`). No commit or push is implied by this
design.

Remote-machine rollout additionally requires `cargo build --workspace`,
`cargo test --workspace`, `cd frontend && bun install && bun run build`, and
`bash scripts/smoke-remote-machines.sh`. Phase G uses Docker Compose under
Colima only. An emulation-only collector keeps `serve` loopback-bound and
unpublished; smoke calls run through `docker compose exec`. One keygen container
creates the ephemeral SSH keypair in tmpfs and provisioning pipes it only into
the collector and SSH-machine tmpfs mounts. Compose file-backed secrets, Swarm,
host key files, host `~/.ssh` mounts, credential bind mounts or volumes, and key
material in writable layers, runtime data, environments, arguments, or logs are
forbidden. Missing Colima, Docker, Compose, host-gateway, or tmpfs prerequisites
are explicit verification limitations, never authorization for a weaker
fallback.

## Risks Requiring Implementation Evidence

- Local native pricing, 5-hour billing block boundaries, directory attribution,
  and reasoning-effort extraction must be verified against representative
  Claude, Codex, and Antigravity JSONL fixtures without regressions.
- Remote SSH v20.0.17+ payload variants must remain verified by sanitized
  fixtures.
- Async/thread safety around Tokio polling, Tauri main-thread tray updates,
  PTY lifecycle, and loopback HTTP shutdown must be verified without data races
  or orphaned child processes.
- Calendar-period boundary tests must establish behavior across local
  time-zone changes and daylight-saving transitions.
- Asset discovery must be tested from explicit `--assets`, Homebrew formula,
  and macOS app-bundle layouts.
- The selected HTTP implementation must preserve loopback-only binding without
  regressing package and release portability.
- SSH argument-boundary tests must prove registry values cannot add commands or
  override the validated destination and remote executable.
- Registry/poller concurrency tests must prove cancelled generations cannot
  publish status after replacement.
- Aggregate tests must prove block/timeline, metric, session, and serialized
  response-row provenance and totals without double counting across machine
  snapshots.
- Registry tests must prove exact version-1/version-2 migration sources and the
  version-3 current envelope, atomic one-way migration, migration-failure
  preservation, normalized persisted defaults, deterministic ordering, and
  fail-closed unknown/version behavior.
- Cache-clear tests must prove per-machine atomicity, cross-machine partial
  results, rollback/store/poller rules, and recovery before and after logical
  publication of an empty cache.
- Cache-path and registry validation must prevent traversal and inline secret
  persistence.
