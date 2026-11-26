# Plan rust-11-cli-binary: Headless Pure-Rust `ccusage-gauge` CLI Binary

- **planId**: `rust-11-cli-binary`
- **dependsOn**: `["rust-10-dashboard-router-http"]`
- **writePaths**:
  - `crates/ccusage-cli/src`
  - `crates/ccusage-cli/tests/cli_tests.rs`
  - `tmp/cargo-target/rust-11-cli-binary`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-11-cli-binary`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/command.md`
  - `design-docs/specs/client-commands.md`
  - `design-docs/specs/design-machine-session-source-directories.md`

## Intent and Repository Context

Implement the headless `ccusage-gauge` CLI binary in `crates/ccusage-cli` using `clap` and `ccusage-core` (with zero GUI/Tauri/WebKit linkage so Linux amd64 builds stay lightweight), preserving every subcommand, flag, text/JSON renderer, bootstrap-logger lifecycle, `dashboard` launcher contract, and exit-code mapping (`0, 1, 2, 3, 4, 5`).

Existing Swift references to imitate:
- `Sources/AppCLI/RootCommand.swift`: subcommand tree, help/version side-effect-free guarantee, lazy bootstrap logger activation ONLY for runtime commands.
- `Sources/AppCLI/Commands/LocalCommands.swift` & `Runtime.swift`: `config-check`, `usage-snapshot [--json]`, `serve [--port] [--assets]`, `dashboard`.
- `Sources/AppCLI/Commands/ClientCommand.swift`, `ClientDashboardCommands.swift`, `ClientMachineCommands.swift`, `ClientOptions.swift`, `ClientRuntime.swift`: full `client` group (`machines list|show|add|update|enable|disable|remove|test-connection|refresh`, `dashboard health|budget|recent|day|period|metrics|cost-series|machine-status|load-status|refresh|cache-clear`).
- `Sources/AppCLI/Rendering/DashboardRenderer.swift` & `MachineRenderer.swift`: compact human-readable text rendering and exact JSON pass-through.

## Non-Goals

- Do NOT reimplement domain rules in `ccusage-cli`; delegate all config, state, scanning, aggregation, HTTP, and validation to `ccusage-core`.
- Do NOT link Tauri or require a local `ccusage` CLI executable for `config-check` or `usage-snapshot`.
- Do NOT touch `Cargo.toml`, `Cargo.lock`, `crates/ccusage-core`, or `src-tauri`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-cli/src/main.rs` & `cli.rs`**:
   - Top-level `clap` parser: `--help`, `--version`, `config-check`, `usage-snapshot [--json]`, `serve [--port <1..=65535>] [--assets <dir>]`, `dashboard`, `client ...`.
   - `--help` and `--version` return `0` without activating `BootstrapLogger` or creating any config/state/cache directories.
   - Invalid CLI arguments / invalid flag values exit `2`.
2. **`crates/ccusage-cli/src/runtime.rs`**:
   - `run_config_check()`: activates `BootstrapLogger`, loads/creates config + validates state, validates `ccusagePath` syntax if non-null WITHOUT requiring a local `ccusage` binary, prints effective status (`dashboardPort`, `dashboardAutostart`, `defaultResetTerm`, `pollIntervalSeconds`).
   - `run_usage_snapshot(json: bool)`: activates `BootstrapLogger`, runs local `SnapshotService` (with `LocalCacheMigrator` and `MachineCacheRecovery`), uses the shared exact half-open `[baseline.activeBoundaryAt, interval.end)` rolling contract (`DESIGN-ROLLING-03` & `DESIGN-ROLLING-10`), emits sorted-keys ISO 8601 JSON when `--json` or human summary otherwise; structured JSON error to stderr + exit `1` on failure in `--json` mode.
   - `run_serve(port: Option<u16>, assets: Option<PathBuf>)`: activates `BootstrapLogger`, loads fail-closed `MachineRegistryStore` (v1/v2->v3 atomic migration), runs `LocalCacheMigrator` + `MachineCacheRecovery` + `DashboardDirectoryNameStore::reconcile_machine_registry`, starts `MachineCollector` and `DashboardHTTPServer` on `127.0.0.1:<port>`, waits for `SIGINT`/`SIGTERM`, stops server and collector cleanly.
   - `run_dashboard()` (`DESIGN-LAUNCH-05`): activates `BootstrapLogger`, calls `ccusage_core::dashboard::resolve_desktop_executable`, spawns `<resolved_gui_exe> --dashboard-only` with inherited environment/stderr, stays in foreground waiting for child exit (or forwarding termination on `SIGINT`/`SIGTERM`), exits `0` when the dashboard window closes or `1` with sanitized diagnostic when executable resolution fails.
3. **`crates/ccusage-cli/src/client.rs` & `rendering.rs`**:
   - Complete `client` subcommand group matching `design-docs/specs/client-commands.md` and `design-machine-session-source-directories.md`:
     - `client machines list|show|add|update|enable|disable|remove|test-connection|refresh` (including repeatable `--codex-session-dir`, `--claude-config-dir`, `--clear-codex-session-dirs`, `--clear-claude-config-dirs`, mutually exclusive include/exclude default flags).
     - `client dashboard health|budget|recent|day|period|metrics|cost-series|machine-status|load-status|refresh|cache-clear`.
   - Exact exit codes: `0` success, `2` usage/argument error, `3` API unreachable, `4` non-5xx rejection or machine action `status == "failed"`, `5` API 5xx or unavailable snapshot/range, `1` local runtime/config error.

## Key Invariants and Pitfalls

- **Zero side effects for `--help`/`--version`**: Verify in tests with isolated empty `CCUSAGE_GAUGE_HOME` that zero files/dirs are created!
- **Exact stream routing**: Successful output goes to stdout; errors and `status: "failed"` machine action results go to stderr with exit `4` (even under `--json`).
- **Executable override precedence**: Never fall back to sibling lookup when `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` is set to an invalid or empty path!

## Test Cases (`crates/ccusage-cli/tests/cli_tests.rs`)

- `ccusage-gauge --help` and `--version` with empty isolated `CCUSAGE_GAUGE_HOME` -> exit `0`, zero files/dirs created.
- `ccusage-gauge config-check` -> creates default config (`0600`) once, succeeds without any `ccusage` binary on `PATH`; relative `ccusagePath` -> exit `1` without modifying file; nonexistent absolute `ccusagePath` -> exit `0`.
- `ccusage-gauge usage-snapshot --json` over native Claude/Codex/Antigravity JSONL fixtures (including rolling `CustomHours(1)` partial bucket split and partial legacy SQLite DB fixture) -> exit `0`, valid JSON matching exact half-open rolling totals (`$2` native split, `$10` partial legacy).
- `ccusage-gauge dashboard` (`DESIGN-LAUNCH-05`):
  - With `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` pointing to a fake executable script verifying `--dashboard-only` arg -> spawns it, waits in foreground, exits `0` when script exits.
  - With `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` pointing to a missing path (even when a sibling `ccusage-gauge-app` exists) -> exits `1` with diagnostic mentioning `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE`, no fallback.
  - With env unset and sibling `ccusage-gauge-app` present -> resolves and launches sibling with `--dashboard-only`.
- `ccusage-gauge client ...` against live in-test loopback `DashboardHTTPServer` -> verifies `health`, `budget`, `metrics`, `machines list/add/update/test-connection/refresh`, exit `3` when unreachable, exit `4` on 4xx or failed machine action, exit `5` on 5xx, exit `2` on invalid flags.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check crates/ccusage-cli/src/*.rs crates/ccusage-cli/tests/cli_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-11-cli-binary cargo test --locked -p ccusage-cli --test cli_tests
```

Done when both commands exit `0` and all CLI subcommand, exit-code, launcher override, and snapshot parity tests pass. (Workspace-wide `cargo clippy` and `cargo build` run in serial Wave 6 join plan `rust-13`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-11-cli-binary` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 5 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-11-cli-binary`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
