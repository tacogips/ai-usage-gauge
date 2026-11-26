# Command-Line Contract

## Status

Updated for the pure Rust + Tauri v2 workspace (see
`design-docs/specs/design-rust-tauri-migration.md`).

## Public Commands

```text
ccusage-gauge --help
ccusage-gauge --version
ccusage-gauge config-check
ccusage-gauge usage-snapshot [--json]
ccusage-gauge serve [--port <port>] [--assets <directory>]
ccusage-gauge dashboard
ccusage-gauge client ...
```

`--help` documents commands without creating configuration or state. `--version`
prints the package version without side effects.

`config-check` resolves the production paths by default, creates the static
configuration only when missing, loads and validates both configuration and
mutable state stores, and reports actionable status. Because local usage
collection uses the native Rust incremental JSONL engine, `config-check` does
not require a local `ccusage` CLI binary to be installed; an existing
`ccusagePath` value is still syntax-validated as an absolute path for backward
compatibility. Test and development builds may expose environment-injected base
paths (`CCUSAGE_GAUGE_HOME`, `CCUSAGE_GAUGE_CONFIG_HOME`,
`CCUSAGE_GAUGE_STATE_HOME`, `CCUSAGE_GAUGE_CACHE_HOME`), while production
defaults remain `~/.config/ccusage-gauge/ccusage-config.json` and
`~/.local/ccusage-gauge/state.json`.

The generated configuration defaults are `ccusagePath: null`,
`defaultResetTerm: daily`, `dashboardPort: 18081`,
`dashboardAutostart: true` (controls whether tray mode opens the in-process
dashboard window on startup), and `pollIntervalSeconds: 20`. `config-check`
reports these effective values but does not rewrite an existing configuration
file.

`usage-snapshot` performs one asynchronous local usage collection through
`ccusage-core` (scanning local Claude Code, Codex, and Antigravity JSONL logs
incrementally and applying the Rust pricing and aggregation engine), then
reports the active reset boundary, cost since reset, and budget summary. It
validates or refreshes the persisted reset baseline first and applies the same
exact half-open `[baseline.activeBoundaryAt, interval.end)` window contract
(inclusive start, exclusive end) used by the tray and `GET /api/budget`: daily,
weekly, and monthly cycles sum daily rows, while `hourly` and rolling
`customHours(n)` cycles select one authoritative input per historical
`(day, agent, model)` partition across both the recent window and any pinned
boundary day—using unfloored priced local event timestamps for complete native
or reconstructible legacy partitions (exact integer token match with cached
`daily_metrics`), and exclusively using unmodified retained SQLite
`session_metrics` rows filtered by `[activeBoundaryAt, interval.end)` (and
stored `directory` when filtered) for partial, unverifiable, or absent-log
legacy partitions—while retaining 15-minute chart buckets in
`dashboardSessions`. Baseline metadata is never treated as monetary usage.
`--json` emits a stable machine-readable object.

`usage-snapshot` remains a local-only compatibility command. Multi-machine
collection is owned by `serve` and `ccusage-gauge-app`; this avoids turning a
one-shot diagnostic into a second collector lifecycle.

`dashboard` launches `ccusage-gauge-app --dashboard-only` to open one tray-free
native Tauri window backed by the in-process `MachineCollector` and
`MachineDashboardRouter` over Tauri IPC without binding a TCP/loopback HTTP
port (always opening the window regardless of `dashboardAutostart`). Executable
resolution preserves the documented override precedence:

1. If `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` is set in the environment (including
   empty), `ccusage-gauge dashboard` validates that exact path as an executable
   file and launches it. It never falls back to sibling lookup when the explicit
   override is missing or not executable, exiting `1` with the existing
   sanitized diagnostic (`Tauri dashboard executable missing. Run mise run
   desktop:build, or set CCUSAGE_GAUGE_DESKTOP_EXECUTABLE.`).
2. When `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` is unset, `ccusage-gauge dashboard`
   resolves the sibling `ccusage-gauge-app` executable beside the canonicalized
   `ccusage-gauge` binary, failing with the same exit-`1` diagnostic if missing
   or non-executable.

`ccusage-gauge dashboard` runs in the foreground, waits for the launched
`--dashboard-only` process, and exits `0` once the window closes (or terminates
the child and exits cleanly on `SIGINT`/`SIGTERM`).

`serve` is a foreground host for the loopback HTTP dashboard service. It binds
only to `127.0.0.1`. `--port` must be in `1...65535`; `--assets` must name a
readable compiled frontend directory and is intended for development and
verification. SIGINT or SIGTERM stops the service cleanly.

The emulation does not relax this listener contract. Docker Compose under
Colima runs an emulation-only collector whose API is not published; smoke calls
execute with `docker compose exec` inside that container against
`http://127.0.0.1:18081`. Its SSH private key arrives by a pipe into container
tmpfs and never through a host file, Compose file-backed secret, bind mount, or
named volume. No proxy, relay, wildcard bind, origin-policy exception, Swarm
deployment, or credential-storage fallback is permitted.

At startup, `serve` loads the machine registry, synthesizes `local`, starts one
background snapshot poller per enabled machine, and exposes machine-aware query,
registry, and health endpoints. Local collection uses the native Rust
incremental JSONL scanner and pricing engine; SSH collection invokes the remote
`ccusage` binary through a direct endpoint or the remote-machine design's
validated direct/jump/command adapter. An operator-opened local forward is a
direct endpoint. `serve` does not create a provider-specific tunnel, deploy
remote software, push data, or persist anything remotely.

Only an absent registry file selects an empty SSH registry. Unsafe directory or
file ownership/type/permissions, malformed JSON, invalid descriptors, or an
unusable persistence path fail startup before the listener or pollers start;
there is no entry quarantine or synthetic-local fallback for a present invalid
file. Recovery is to stop the service, repair or intentionally remove the file,
and restart.
The persisted document is the closed version-3
`schemaVersion`/`localSessionSources`/`machines` envelope from the
session-source design. Exact valid version-1 and version-2 documents are
atomically migrated before listener or poller startup; migration failure keeps
the original bytes and fails startup. Missing or unsupported versions and
duplicate or unknown fields fail closed; API defaults are normalized before
the canonical version-3 document is written.

One serialized registry owner validates and stages the complete candidate,
synchronizes an atomic mode-`0600` save, reconciles the affected poller
generation and snapshot/status state, then publishes the advanced revision
before responding. A reconciliation failure compensates disk and runtime back
to the prior revision; failed compensation latches
`registry_reconciliation_required` until controlled restart recovery. Invalid
descriptors, attempts to mutate `local`, unsafe machine ids or SSH options, and
inline secret material leave both persisted and running state unchanged.
Registry mutations, `GET /api/refresh`, and `DELETE /api/cache` require the
remote-machine design's loopback/same-origin gate and
`X-CCUsage-Gauge-Mutation: 1`; rejected requests perform no work.

### Remote-machine observability actions

The existing `client` command group adds:

```text
ccusage-gauge client machines test-connection <id> [--api-port <port>] [--json]
ccusage-gauge client machines refresh <id> [--api-port <port>] [--json]
```

Both commands call the guarded additive routes defined in
`design-docs/specs/design-remote-machine-collection.md` and set
`X-CCUsage-Gauge-Mutation: 1`. Test connection reports `reachable` or `failed`
plus the sanitized diagnostic and safe remediation. Refresh reloads the
registry, recollects the selected enabled machine immediately, and reports
refreshed/failed ids. Neither command accepts raw SSH arguments or a command to
execute. A decoded action result with `status: "failed"` is written to stderr
and exits `4`; successful `reachable` or `ok` results are written to stdout and
exit `0`. `--json` preserves the exact action response on the corresponding
stream.

`config-check`, `usage-snapshot`, `serve`, and client runtime failures use the
persistent bootstrap logger defined in `design-docs/specs/architecture.md`.
Runtime commands activate it before configuration parsing; help and version do
not activate it and remain side-effect free. The active file is still created
lazily on the first record. CLI stderr and JSON error behavior remain
authoritative for the caller even when persistent logging is unavailable.

## Exit and Error Behavior

- `0`: requested operation completed successfully.
- `2`: invalid command, option, or user-supplied value.
- `3`: a `client` command could not reach the dashboard API.
- `4`: a `client` command received an actionable non-5xx rejection or a
  completed machine action with `status: "failed"`.
- `5`: a `client` command received an API 5xx or unavailable snapshot/range.
- `1`: configuration, state, `ccusage`, asset, bind, decoding, or other runtime
  failure.

Human-readable errors go to standard error and identify the failed boundary
without printing raw usage output or environment contents. JSON output includes a
machine-readable error code when JSON mode was requested. A relative or
malformed `ccusagePath` fails configuration validation without modifying the
configuration file.
`design-docs/specs/client-commands.md` remains authoritative for the complete
client-command error-stream and exit-status mapping.

The CLI reuses `ccusage-core` configuration, persistence, validation,
aggregation, and HTTP behavior. It must not become a second implementation of
product rules.
