# Plan rust-13-tooling-scripts-packaging: Mise Tasks, Packaging/Homebrew Scripts, Smoke Suites, CI, and Emulation

- **planId**: `rust-13-tooling-scripts-packaging`
- **dependsOn**: `["rust-11-cli-binary", "rust-12-tauri-tray-app"]`
- **writePaths**:
  - `mise.toml`
  - `Resources/MenuBarInfo.plist`
  - `Resources/DashboardInfo.plist`
  - `scripts`
  - `.github/workflows/linux-amd64-build.yml`
  - `deploy/emulation/Dockerfile.collector`
  - `target`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `target`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/command.md`

## Intent and Repository Context

Migrate all build automation (`mise.toml`), macOS app bundle staging, Homebrew Formula & Cask release scripts, smoke/E2E/coverage scripts, GitHub Actions Linux amd64 workflow, and Docker emulation collector image from SwiftPM to the pure Rust + Tauri v2 workspace (`ccusage-gauge` + `ccusage-gauge-app`), remove obsolete Swift-only scripts and `Resources/DashboardInfo.plist`, and verify the updated smoke and packaging dry-run gates against the live compiled Rust binaries.

Existing files to inspect before editing:
- `mise.toml`, `Resources/MenuBarInfo.plist`, `.gitignore`, `.github/workflows/linux-amd64-build.yml`, `deploy/emulation/Dockerfile.collector`.
- `scripts/*` (17 files: update 13, delete 4 Swift-only scripts: `scripts/render-app-icon.swift`, `scripts/stage-desktop-app.sh`, `scripts/build-desktop-release.sh`, `scripts/sync-frontend-assets.sh`).

## Non-Goals

- Do NOT delete `Package.swift`, `Package.resolved`, `.swiftlint.yml`, `Sources/`, `Tests/`, or edit top-level markdown docs here (handled in `rust-14-swift-removal-and-docs`).
- Do NOT publish releases, push tags, or touch credentials/notarization secrets; run Homebrew release scripts in `--dry-run` mode only.
- Do NOT modify `frontend/` or any Rust source files.

## Target Files, Symbols, and Contracts

1. **`mise.toml`**:
   - `[tools]`: remove `swift` and `swiftlint`; retain `rust = "1.98.1"`, `bun = "1.4.4"`, `gitleaks`, `github-cli`, `pre-commit`.
   - Tasks:
     - `build`: depends `["frontend:build"]`, runs `cargo build --workspace`
     - `test`: runs `cargo test --workspace`
     - `lint`: runs `cargo clippy --workspace --all-targets -- -D warnings`
     - `run`: runs `cargo run -p ccusage-cli -- "$@"`
     - `clean`: `rm -rf target dist release`
     - `frontend:build`: `bun run build` only (remove `../scripts/sync-frontend-assets.sh` call!)
     - `desktop:build`: depends `["frontend:build"]`, builds `ccusage-cli` and `ccusage-gauge-app` release binaries (`cargo build --release -p ccusage-cli -p ccusage-gauge-app` — both already land side-by-side in `target/release/`!)
     - `desktop:run`: depends `["desktop:build"]`, runs `target/release/ccusage-gauge dashboard`
     - `desktop:test`: depends `["frontend:build"]`, runs `cargo test -p ccusage-gauge-app`
     - `app:build` / `app:run`: builds `target/CCUsageGauge.app` via `scripts/build-local-app.sh` and opens `target/CCUsageGauge.app`.
2. **`Resources/MenuBarInfo.plist` & delete `Resources/DashboardInfo.plist`**:
   - Update `CFBundleExecutable` to `ccusage-gauge-app`, version to `0.5.1`; remove obsolete nested helper plist `Resources/DashboardInfo.plist`.
3. **`scripts/` updates & deletions**:
   - Delete Swift-only scripts: `scripts/render-app-icon.swift`, `scripts/stage-desktop-app.sh`, `scripts/build-desktop-release.sh`, `scripts/sync-frontend-assets.sh`.
   - `scripts/build-local-app.sh` & `scripts/build-e2e-app.sh`: stage `target/CCUsageGauge.app` with `Contents/MacOS/ccusage-gauge-app`, `Contents/MacOS/ccusage-gauge`, `Contents/Resources/AppIcon.icns`, `Contents/Resources/Web/` (copied from `frontend/dist`), and `Contents/Info.plist` (zero nested helper app!).
   - `scripts/build-homebrew-release.sh` & `scripts/render-homebrew-formula.sh`: replace `Sources/AppCore/Version.swift` version check with `Cargo.toml` (`[workspace.package] version = "..."`) parity check; build Rust release targets (`aarch64-apple-darwin`, `x86_64-apple-darwin`) for `ccusage-gauge` and `ccusage-gauge-app`; stage `bin/ccusage-gauge`, `bin/ccusage-gauge-app`, `share/ccusage-gauge/web`, `README.md`, `LICENSE`; update `render-homebrew-formula.sh` to install `bin/ccusage-gauge` and `bin/ccusage-gauge-app` (replacing `bin/ccusage-gauge-dashboard`); keep `--dry-run` working.
   - `scripts/build-homebrew-cask-release.sh`, `scripts/render-homebrew-cask.sh`, `scripts/release-homebrew-cask-local.sh`: replace `Sources/AppCore/Version.swift` version check with `Cargo.toml` parity check; stage `CCUsageGauge.app` with `Contents/MacOS/ccusage-gauge-app` and `Contents/MacOS/ccusage-gauge` (no nested helper); Developer ID sign/notarize/staple DMG; update `render-homebrew-cask.sh` to remove `"CCUsageGaugeDashboard.app/Contents/MacOS/ccusage-gauge-dashboard"` while retaining `app "CCUsageGauge.app"` and `binary "#{appdir}/CCUsageGauge.app/Contents/MacOS/ccusage-gauge", target: "ccusage-gauge"`; keep `--dry-run` working.
   - `scripts/smoke-isolated-runtime.sh`: resolve `target/debug/ccusage-gauge` (or `target/release/ccusage-gauge`); seed native Claude/Codex JSONL fixtures under isolated `CCUSAGE_GAUGE_*`/`CLAUDE_CONFIG_DIR`/`CODEX_HOME` with NO `ccusage` on `PATH` (and place a sentinel `$marker` `ccusage` on `PATH` to assert it is NEVER executed); verify `config-check` and `usage-snapshot --json` succeed both with default `ccusagePath: null` AND with a nonexistent absolute `ccusagePath: "/definitely/missing/ccusage"` without touching `$marker`; then write a **relative** `ccusagePath: "relative/ccusage"` and assert both commands exit `1` without mutating config or touching real `$HOME/.config` / `$HOME/.local`.
   - `scripts/smoke-packaged-assets.sh`: update layouts to `formula|cask|all` (removing the obsolete `swiftpm` `Bundle.module` layout rather than inventing an unsupported `cargo` packaged layout, matching `architecture.md:906-912` and `rust-10` `StaticAssetResolver`), resolve `target/debug/ccusage-gauge` (or `target/release/ccusage-gauge`) + `frontend/dist`, remove fake local `ccusage` binary/config assumptions (keeping isolated `CCUSAGE_GAUGE_CONFIG_HOME` / `CCUSAGE_GAUGE_STATE_HOME` / `CCUSAGE_GAUGE_CACHE_HOME`), and verify both valid serving (`200` containing `ccusage-gauge`) and `--expect-missing-diagnostics` (`503 assets_missing`) across `formula` (`share/ccusage-gauge/web` relative to `<stage>/bin/ccusage-gauge`) and `cask` (`Contents/Resources/Web` relative to `<stage>/CCUsageGauge.app/Contents/MacOS/ccusage-gauge`).
   - `scripts/smoke-dashboard.sh`: resolve `target/debug/ccusage-gauge` (or `target/release/ccusage-gauge`) and seed local native JSONL fixture instead of fake local `ccusage` binary while keeping route, shutdown, and same-port restart assertions intact.
   - `scripts/smoke-remote-machines.sh` & `scripts/smoke-remote-machines-real.sh`: keep remote SSH `ccusage` emulation intact while updating local collector binary build/references to Cargo.
   - `scripts/test-coverage.sh`: replace `swift test --enable-code-coverage` with Rust coverage over `crates/ccusage-core` and `crates/ccusage-cli` (via `cargo llvm-cov` or `RUSTFLAGS="-C instrument-coverage"` + `llvm-cov`, failing clearly when `llvm-cov` is unavailable).
4. **`.github/workflows/linux-amd64-build.yml`**:
   - Replace Swift container image with Rust `1.98.1` + Bun `1.4.4` build steps running `cargo test -p ccusage-core -p ccusage-cli`, `cargo build --release -p ccusage-cli --target x86_64-unknown-linux-gnu`, `cd frontend && bun install --frozen-lockfile && bun run build`, staging `bin/ccusage-gauge` + `share/ccusage-gauge/web`, and running `scripts/smoke-dashboard.sh`.
5. **`deploy/emulation/Dockerfile.collector`**:
   - Replace `FROM swift:6.0-jammy AS builder` with `FROM rust:1.98-bookworm AS builder` copying root `Cargo.toml`, `Cargo.lock`, `crates/`, and `src-tauri/` (or `src-tauri/Cargo.toml` plus stub `src-tauri/src/lib.rs`, `src-tauri/src/main.rs`, `src-tauri/build.rs`) so `cargo build --locked --release -p ccusage-cli` resolves the root Cargo workspace cleanly without failing on missing workspace member `src-tauri`.
   - Retain the existing `FROM node:22.23.4-bookworm-slim AS ccusage-runtime` (`ccusage@20.0.17` oracle) stage, and replace the final `FROM swift:6.0-jammy-slim` stage with `FROM debian:bookworm-slim` (`ca-certificates curl openssh-client`, `collector` user uid `1000`, `COPY --from=builder /src/target/release/ccusage-gauge /usr/local/bin/ccusage-gauge`), preserving `/usr/local/bin/node`, `/usr/local/lib/node_modules`, `/usr/local/bin/ccusage.real` (`ccusage 20.0.17`), `collector-ccusage-stub.sh`, `ccusage-dispatcher.sh`, and `collector-entrypoint.sh` so `deploy/emulation/assert-real-parity.py` (`scripts/smoke-remote-machines-real.sh`) continues passing.

## Key Invariants and Pitfalls

- **Single-process bundle layout**: `CCUsageGauge.app` MUST NOT contain `Contents/Helpers/CCUsageGaugeDashboard.app`; it contains `Contents/MacOS/ccusage-gauge-app` (`CFBundleExecutable`) and sibling `Contents/MacOS/ccusage-gauge`.
- **Packaged web assets**: Both `share/ccusage-gauge/web` (Formula/Linux) and `Contents/Resources/Web` (Cask) copy directly from `frontend/dist` (no `Sources/AppCore/Resources/Web`).
- **Bash syntax & executable bits**: Keep `chmod +x` on all shell scripts and pass `bash -n scripts/*.sh`.

## Test Cases and Verification Commands

```bash
bash -n scripts/*.sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --workspace
scripts/smoke-isolated-runtime.sh
scripts/smoke-dashboard.sh --port 18181 --assets frontend/dist
scripts/smoke-packaged-assets.sh --layout all
scripts/smoke-packaged-assets.sh --layout all --expect-missing-diagnostics
scripts/build-homebrew-release.sh --dry-run
scripts/build-homebrew-cask-release.sh --dry-run
```

Done when:
1. All commands above exit `0` (including the serial Wave 6 workspace clippy + build gate joining Wave 5).
2. The 4 Swift-only scripts and `Resources/DashboardInfo.plist` are removed.
3. No script references `swift`, `swiftlint`, `.build/`, `Sources/AppCore/Version.swift`, `Sources/AppCore/Resources/Web`, or `CCUsageGaugeDashboard.app`.

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `target` (added to `writePaths` and `artifactRoots`, gitignored via `/target/`) authorizes Cargo clippy/build outputs in `target/debug/` and `target/release/` consumed by the updated smoke and Homebrew dry-run scripts.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; external OS `$TMPDIR` (`mktemp -d`) authorized for smoke scripts. Never stage or commit `target/`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
