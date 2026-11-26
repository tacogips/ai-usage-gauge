# Plan rust-12-tauri-tray-app: Single-Process Tauri v2 macOS Menu-Bar Tray and Embedded Dashboard Application

- **planId**: `rust-12-tauri-tray-app`
- **dependsOn**: `["rust-08-tray-icon-renderer", "rust-10-dashboard-router-http"]`
- **writePaths**:
  - `src-tauri/tauri.conf.json`
  - `src-tauri/build.rs`
  - `src-tauri/src/lib.rs`
  - `src-tauri/src/main.rs`
  - `src-tauri/src/bridge.rs`
  - `src-tauri/src/ipc.rs`
  - `src-tauri/src/tray_menu.rs`
  - `src-tauri/src/dialogs.rs`
  - `src-tauri/src/launch_at_login.rs`
  - `src-tauri/tests/ipc_and_menu_tests.rs`
  - `tmp/cargo-target/rust-12-tauri-tray-app`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-12-tauri-tray-app`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/design-subscription-rate-limits.md`

## Intent and Repository Context

Replace the legacy Swift-Tauri stdin/stdout pipe bridge (`src-tauri/src/bridge.rs`) and `CCUsageGaugeMenuBar` with the single-process pure-Rust Tauri v2 desktop application (`ccusage-gauge-app`) owning:
1. Tray mode (`ActivationPolicy::Accessory` on macOS): live RGBA template icon (`TrayIcon::set_icon_with_as_template`), total cost title (`TrayIcon::set_title`), tooltip (`TrayIcon::set_tooltip`) with anti-flicker change detection, full native `tauri::menu` with live `MenuItem::set_text` / `CheckMenuItem` updates, internal `ccusage-dialog://` settings dialogs, pure-Rust `SMAppService.mainAppService` Launch at Login, and `dashboardAutostart` (`true` opens `"main"` window at startup; `false` keeps it closed until requested; closing window hides/closes it without quitting).
2. Dashboard-only mode (`--dashboard-only`, invoked by `ccusage-gauge dashboard`): opens one tray-free `"main"` `WebviewWindow` unconditionally (ignoring `dashboardAutostart`) with zero TCP listener, exiting cleanly when the window closes or on `SIGINT`/`SIGTERM`.
3. In-process `dashboard_request` Tauri IPC command dispatching directly to `MachineDashboardRouter` so `frontend/src/desktopTransport.ts` works unmodified.

Existing Swift references to imitate:
- `Sources/CCUsageGaugeMenuBar/MenuBarApp.swift` & `MenuBarApp+PieCharts.swift`: tray menu structure, budget summary formatting, `$12`/`$—`/`$!`/`$12 !` title formatting, tooltip layout (including credit lines), `dashboardAutostart` check (`MenuBarApp.swift:274`), error-details submenu + `Retry validation`, E2E env hook `CCUSAGE_GAUGE_E2E_OPEN_MENU=1`.
- `Sources/CCUsageGaugeMenuBar/BudgetEditorView.swift` & `MenuBarDisplaySettings.swift`: exact settings controls and state persistence rules.
- `Sources/CCUsageGaugeMenuBar/LaunchAtLogin.swift`: `SMAppService.mainAppService` states (`off`, `on`, `requiresApproval`, `unavailable`).
- `src-tauri/src/bridge.rs` & `src-tauri/src/main.rs`: `dashboard_request` IPC command contract (`Request { path, method, body }`, `Response { status, contentType, body }`, window label `"main"`, `/api/` prefix, body `<= 1 MiB`).

## Non-Goals

- Do NOT modify `frontend/` (keep `frontend/src/desktopTransport.ts` working as-is).
- Do NOT bind a TCP listener in `ccusage-gauge-app`.
- Do NOT touch `src-tauri/Cargo.toml`, `src-tauri/assets`, or `src-tauri/src/tray_icons.rs`.
- Remove `src-tauri/src/bridge.rs` cleanly.

## Target Files, Symbols, and Contracts

1. **`src-tauri/tauri.conf.json` & `src-tauri/build.rs`**:
   - Update `productName` to `"CCUsage Gauge"`, `identifier` to `"com.tacogips.ccusage-gauge"`, `version` to `"0.5.1"`, keep `build.frontendDist: "../frontend/dist"`, keep CSP compatible with embedded assets and `ipc: http://ipc.localhost`, configure `"main"` window as `visible: false` (or created dynamically on demand / when `dashboardAutostart == true` or `--dashboard-only`) so tray mode with `dashboardAutostart: false` NEVER flashes a window on startup!
2. **`src-tauri/src/ipc.rs`** (and delete `src-tauri/src/bridge.rs`):
   - `DashboardIpcRequest { path, method, body }` & `DashboardIpcResponse { status, content_type, body }` (`#[serde(rename_all = "camelCase")]`).
   - Validation matching `design-rust-tauri-migration.md:181`: `window.label() == "main"`, `path.starts_with("/api/")` without `\r`/`\n`/`#`, `method` in `{"GET", "POST", "PUT", "PATCH", "DELETE"}`, `body.len() <= 1_048_576`.
   - `dispatch_ipc_request(router, request)` calling `MachineDashboardRouter` directly with trusted in-process mutation authorization (no HTTP socket).
3. **`src-tauri/src/tray_menu.rs`**:
   - Pure-logic `TrayViewModel` (testable headless without a display!) computing:
     - `TrayRenderState` for `TrayIconRenderer::render_strip` (canonical `PieChartId` order, budget pie, rate limits, inline ratio labels).
     - `status_title` (`$12`, `$—`, `$!`, `$12 !`, or `""` when `menuBarShowCost == false`).
     - `tooltip` (warning/error text on failure, else selected chart summaries + active `Claude Code credit` / `Codex credit` lines).
     - Budget summary line, conditional Error Details submenu items, rate-limit `compact_menu_line` items, `Usage icons` `CheckMenuItem`s (disabling sole remaining checked icon), `Set budget…`, `Aggregation period` (`Hourly`, `Daily`, `Weekly`, `Monthly`, `Custom hours…`), `Refresh interval` (live `Every N seconds`, `Set seconds…`, conditional `Use config default (N seconds)`), `Settings` (`Launch at Login` + conditional approval guidance, `Menu bar display…`, inline `Ratio text` submenu), `Open dashboard`, dynamic `Open dashboard window` / `Close dashboard`, `Refresh`, `Quit`.
   - Structural-change fingerprint vs steady-state live updates: rebuild `tauri::menu::Menu` only on structural change (selected charts or error state changed); steady-state polls call only `MenuItem::set_text` and `CheckMenuItem::set_checked`, and `TrayPresentationDedup` gates `TrayIcon::set_icon_with_as_template`, `set_title`, and `set_tooltip`.
4. **`src-tauri/src/dialogs.rs`**:
   - Registers custom in-app URI scheme `ccusage-dialog://localhost/<dialog>` (`budget`, `custom-hours`, `refresh-interval`, `display-settings`) serving self-contained HTML/JS dialog pages (without touching `frontend/`) with Tauri IPC save commands that validate inputs, persist to `StateStore`, refresh `AppState` + baseline, and immediately update the tray.
5. **`src-tauri/src/launch_at_login.rs`**:
   - `LaunchAtLoginState` (`Off`, `On`, `RequiresApproval`, `Unavailable`).
   - macOS implementation using `objc2` runtime FFI linking `ServiceManagement.framework` (`#[link(name = "ServiceManagement", kind = "framework")]`) invoking `SMAppService.mainAppService` (`status`, `registerAndReturnError:`, `unregisterAndReturnError:`); in-memory mock when `CCUSAGE_GAUGE_E2E_OPEN_MENU=1`; `Unavailable` stub on non-macOS.
6. **`src-tauri/src/lib.rs` & `src-tauri/src/main.rs`**:
   - Exports `pub mod tray_icons; pub mod ipc; pub mod tray_menu; pub mod dialogs; pub mod launch_at_login;`.
   - CLI mode flag: `--dashboard-only` (reject unknown flags with exit `2`).
   - Tray mode: activates `BootstrapLogger`, loads config/state/registry + migrations/reconciliation, starts `MachineCollector` & `RateLimitUsageService` & background polling loop, sets `ActivationPolicy::Accessory` on macOS, creates tray icon, and checks `config.dashboard_autostart` (`DESIGN-LAUNCH-05`: `true` -> show `"main"` window immediately; `false` -> keep window closed until tray menu request). Window close event intercepts close in tray mode (`api.prevent_close(); window.hide()`) and returns to accessory mode without stopping polling or quitting.
   - Dashboard-only mode (`--dashboard-only`): no tray icon, opens `"main"` window unconditionally regardless of `dashboardAutostart`, exits `0` cleanly when `"main"` closes or on `SIGINT`/`SIGTERM` after stopping collector.

## Key Invariants and Pitfalls

- **`dashboardAutostart: false` must NOT flash a window**: Ensure `"main"` is hidden by default in `tauri.conf.json` and only shown when `dashboard_only || config.dashboard_autostart`!
- **Main-thread Tauri calls**: Dispatch tray/menu/window mutations via `app_handle.run_on_main_thread(...)` from Tokio polling tasks.
- **No flicker on steady-state polls**: Never rebuild the tray menu or re-upload the RGBA icon when `TrayPresentationDedup` reports unchanged inputs.

## Test Cases (`src-tauri/tests/ipc_and_menu_tests.rs`)

- `DashboardIpcRequest` validation -> accepts `/api/health`, `/api/metrics?machine=all`, `GET`/`POST`/`PUT`/`PATCH`/`DELETE`; rejects non-`/api/`, `\r`/`\n`/`#`, `OPTIONS`, and body `> 1 MiB`.
- `dispatch_ipc_request` against real in-memory `MachineDashboardRouter` -> routes `GET /api/health` (`200`), `PATCH /api/machines/local` (`200`, trusted in-process mutation without HTTP), `GET /api/budget` (matching exact half-open rolling contract).
- `TrayViewModel` tests -> title formatting (`$12`, `$—`, `$!`, `$12 !`, `""`), tooltip with credit lines vs error state, sole-remaining checked `Usage icons` disabled invariant, dynamic `Open dashboard window` vs `Close dashboard` label, structural-change vs steady-state-text-only update classification.
- `LaunchAtLogin` mock mode (`CCUSAGE_GAUGE_E2E_OPEN_MENU=1`) -> toggles `Off` <-> `On` cleanly without touching macOS `SMAppService`.
- Dialog input validation -> rejects negative USD budget, `0` custom hours, `0` refresh seconds; accepts valid values and updates `StateStore`.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check src-tauri/src/lib.rs src-tauri/src/main.rs src-tauri/src/ipc.rs src-tauri/src/tray_menu.rs src-tauri/src/dialogs.rs src-tauri/src/launch_at_login.rs src-tauri/tests/ipc_and_menu_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-12-tauri-tray-app cargo test --locked -p ccusage-gauge-app --test ipc_and_menu_tests
```

Done when both commands exit `0`, `src-tauri/src/bridge.rs` is removed, and all IPC, tray-model, dialog-validation, and launch-at-login tests pass. (Workspace-wide `cargo clippy` and `cargo build` run in serial Wave 6 join plan `rust-13`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-12-tauri-tray-app` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 5 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-12-tauri-tray-app`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
