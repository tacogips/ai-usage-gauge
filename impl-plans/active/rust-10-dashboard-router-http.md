# Plan rust-10-dashboard-router-http: DashboardQueryService, MachineDashboardRouter, Loopback HTTP Server, API Client, and Desktop Executable Resolver

- **planId**: `rust-10-dashboard-router-http`
- **dependsOn**: `["rust-06-rate-limits", "rust-09-local-aggregation-snapshot"]`
- **writePaths**:
  - `crates/ccusage-core/src/dashboard`
  - `crates/ccusage-core/tests/dashboard_router_tests.rs`
  - `target`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `target`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/client-commands.md`
  - `design-docs/specs/design-remote-machine-collection.md`
  - `design-docs/specs/design-dashboard-dark-flat-effort-grouping.md`

## Intent and Repository Context

Implement the entire dashboard query, routing, security-gating, static asset, loopback HTTP server/client, and desktop GUI executable-resolution layer in `ccusage-core` so both Tauri in-process IPC (`rust-12`) and `ccusage-gauge serve` / `client` / `dashboard` (`rust-11`) share one authoritative implementation.

Existing Swift references to imitate:
- `Sources/AppCore/DashboardQuery.swift`: `DashboardQueryService` (`/api/recent`, `/api/day`, `/api/period`, `/api/metrics`, `/api/cost-series`, `/api/budget` with `directory`, `directoryBreakdown`, `effortBreakdown`).
- `Sources/AppCore/MachineDashboardRouter.swift`, `MachineDashboardRouter+DirectoryQueries.swift`, `MachineDashboardRouter+MachineRoutes.swift`, `MachineDashboardRouter+SessionSources.swift`: all `/api/*` routes, machine selection (`all` vs repeated ids, `400`/`404`/`409`/`503`), partial `200` / `207` / `500` cache clear, snapshot read coalescing, and fail-closed mutation gates.
- `Sources/AppCore/HTTPService.swift`, `HTTPService+DashboardState.swift`, `HTTPService+DirectoryNames.swift`, `HTTPService+RateLimits.swift`, `DashboardHTTPServer.swift`: loopback `127.0.0.1` binding, `StaticAssetResolver` (explicit `--assets` -> adjacent `share/ccusage-gauge/web` -> macOS `Contents/Resources/Web` -> `503 assets_missing`), SPA fallback for non-API paths, no directory listing.
- `Sources/AppCore/DashboardAPIClient.swift`: typed loopback client for CLI `client` subcommands.
- `Sources/AppCore/DesktopDashboard.swift:executableURL`: `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` override precedence + sibling `ccusage-gauge-app` lookup + diagnostic without fallback on invalid explicit override.

## Non-Goals

- Do NOT bind `0.0.0.0`, IPv6 any-address, or a configurable network interface; strictly `127.0.0.1`.
- Do NOT restore the Swift stdin/stdout pipe protocol or nested `CCUsageGaugeDashboard.app` helper.
- Do NOT modify `frontend/`, `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/dashboard/mod.rs`**: re-exports `query`, `router`, `assets`, `http_server`, `api_client`, `desktop_executable`.
2. **`crates/ccusage-core/src/dashboard/query.rs`**:
   - `DashboardQueryService`:
     - `recent`, `day`, `period` (preset & custom): backed by `CostSnapshot.points` with cold/warm-cache parity.
     - `metrics`: unfiltered daily path vs `recent12h` / directory-filtered session rollup path; preserves exact totals.
     - `cost_series`: `15min`/`hourly`/`6hour` from 15-minute chart buckets (`dashboardSessions`), `daily` from `dashboardMetrics` (or session rollup when `directoryBreakdown=true`, `effortBreakdown=true`, or directory filters are active); strict boolean parsing (`true`/`false`, duplicate -> `400`).
     - `budget`: uses the **exact same `selected_period_cost` half-open `[activeBoundaryAt, end)` engine from `rust-09`** both when unfiltered AND when filtered by repeatable `directory=<machine>:<path>` (preserving partial boundary buckets and unified legacy partition precedence from `DESIGN-ROLLING-03` & `DESIGN-ROLLING-10`!).
3. **`crates/ccusage-core/src/dashboard/router.rs`**:
   - `MachineDashboardRouter::route(req: RouterRequest) -> RouterResponse` (used identically by `DashboardHTTPServer` and Tauri in-process `dashboard_request` IPC).
   - Routes: `GET /api/health`, `GET /api/chart-colors`, `GET /api/rate-limits`, `GET /api/recent`, `GET /api/day`, `GET /api/period`, `GET /api/metrics`, `GET /api/cost-series`, `GET /api/budget`, `GET /api/subdirectories`, `PUT /api/subdirectories/name`, `GET/POST /api/machines`, `GET/PUT/PATCH/DELETE /api/machines/{id}`, `POST /api/machines/{id}/test-connection`, `POST /api/machines/{id}/refresh`, `GET /api/machine-status`, `GET /api/load-status`, `GET /api/refresh`, `DELETE /api/cache`, `GET/PUT /api/dashboard-state`.
   - Fail-closed security gate on HTTP control/mutation routes (running BEFORE body decoding, validation, or store access): exact loopback `Host` (`127.0.0.1:<port>` or `localhost:<port>`), exact same-origin `Origin` / `Sec-Fetch-Site` check when browser headers are present, required `X-CCUsage-Gauge-Mutation: 1`, `OPTIONS` rejected with no CORS headers, trusted in-process flag for Tauri IPC (`dashboard_request`).
   - `GET /api/subdirectories`: returns full-string-sorted directories per machine and optional `names` map containing ONLY currently discovered directories (omitted when empty); fails `503` on store read error rather than falling back silently.
   - `PUT /api/subdirectories/name`: returns `400 invalid_directory_name`, `404 machine_not_found` (including active deletion barrier), `503 directory_name_unavailable`, never logging untrusted paths/names.
   - `DELETE /api/cache`: per-machine atomic clear returning `200`, `207`, or `500` with per-item `cache_failed`.
4. **`crates/ccusage-core/src/dashboard/assets.rs`**:
   - `StaticAssetResolver`: resolves explicit `--assets` -> adjacent `share/ccusage-gauge/web` -> macOS bundle `Contents/Resources/Web`; rejects path traversal (`..`); serves SPA `index.html` fallback for non-API navigation paths; returns `503 assets_missing` JSON diagnostic when absent.
5. **`crates/ccusage-core/src/dashboard/http_server.rs` & `api_client.rs`**:
   - `DashboardHTTPServer`: `tokio::net::TcpListener::bind(("127.0.0.1", port))` + `hyper` HTTP/1 server with graceful shutdown handle (`stop()`), deterministic close allowing immediate same-port restart.
   - `DashboardAPIClient`: typed HTTP client over `127.0.0.1:<port>` setting `X-CCUsage-Gauge-Mutation: 1` on mutation calls and mapping network vs 4xx vs 5xx errors for CLI exit codes `3`, `4`, `5`.
6. **`crates/ccusage-core/src/dashboard/desktop_executable.rs`**:
   - `resolve_desktop_executable(env, cli_exe_path) -> Result<PathBuf, DesktopExecutableError>` (`DESIGN-LAUNCH-05`):
     1. If `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` is present in `env` (even if empty string!), validate that exact path is an executable file (`is_file() && mode & 0o111 != 0`); if invalid/missing, return `DesktopExecutableError::ExecutableMissing` **without falling back** to sibling lookup!
     2. If unset, canonicalize `cli_exe_path` and look for sibling `ccusage-gauge-app` in the same directory; if missing/non-executable, return `DesktopExecutableError::ExecutableMissing` with exact diagnostic `"Tauri dashboard executable missing. Run mise run desktop:build, or set CCUSAGE_GAUGE_DESKTOP_EXECUTABLE."`.

## Key Invariants and Pitfalls

- **Gate ordering**: Mutation/origin/header checks MUST run before JSON body decoding so unauthorized callers cannot probe schema or trigger store access.
- **Directory query encoding**: Percent-decode `directory=<machine>:<full-path>` splitting at the FIRST colon; ignore well-formed filters for known machines outside the active `machine` selection; reject malformed or unknown machine ids with `400`/`404` without echoing untrusted directory strings.
- **Zero TCP socket for native IPC**: `MachineDashboardRouter` has no dependency on `DashboardHTTPServer`.

## Test Cases (`crates/ccusage-core/tests/dashboard_router_tests.rs`)

- All `/api/*` read routes (`/health`, `/chart-colors`, `/rate-limits`, `/recent`, `/day`, `/period`, `/metrics`, `/cost-series`, `/budget`, `/subdirectories`, `/machine-status`, `/load-status`, `/dashboard-state`) -> exact JSON field names and status codes.
- `/api/budget` with rolling `CustomHours(1)` and partial boundary bucket + `directory` filter + partial legacy partition -> matches tray and CLI exact half-open totals (`$2` native split, `$10` partial legacy, stored-directory filtering, exclusive end).
- `/api/cost-series` with `directoryBreakdown=true`, `effortBreakdown=true`, invalid/duplicate boolean (`400`), `machine=all` mixed with concrete id (`400`), unknown id (`404`), disabled id (`409`).
- Mutation gate tests on `PUT /api/subdirectories/name`, `POST /api/machines`, `GET /api/refresh`, `DELETE /api/cache`: missing `X-CCUsage-Gauge-Mutation: 1`, bad `Host`, cross-origin `Origin`, `OPTIONS` -> rejected before decoding/persistence with no state change.
- `DashboardHTTPServer` start on ephemeral `127.0.0.1:0`, serve static assets + SPA fallback + `503 assets_missing` when absent, round-trip via `DashboardAPIClient`, graceful stop + immediate rebind on same port.
- `resolve_desktop_executable` (`DESIGN-LAUNCH-05`): valid explicit `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` wins over sibling; missing/non-executable/empty explicit override fails WITHOUT falling back even when valid sibling exists; unset env resolves sibling `ccusage-gauge-app`.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --edition 2021 --check crates/ccusage-core/src/dashboard/*.rs crates/ccusage-core/tests/dashboard_router_tests.rs
cargo test --locked -p ccusage-core --test dashboard_router_tests
cargo clippy --locked -p ccusage-core --all-targets -- -D warnings
```

Done when all three commands exit `0` and all router, query, security-gate, HTTP server/client, static-asset, and desktop-executable tests pass.

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `target` (added to `writePaths` and `artifactRoots`, gitignored via `/target/`) authorizes Cargo test and clippy build outputs for this serial Wave 4 join plan.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `target/`.

## Amendment 2026-11-27 (blocked-wave-scope-or-contract, round 2)

- **Workspace edition on standalone `rustfmt` gate**: Updated verification command to `rustfmt --edition 2021 --check crates/ccusage-core/src/dashboard/*.rs crates/ccusage-core/tests/dashboard_router_tests.rs` matching `[workspace.package] edition = "2021"` so `async` syntax in `http_server.rs` / `api_client.rs` / `router.rs` is parsed cleanly.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
