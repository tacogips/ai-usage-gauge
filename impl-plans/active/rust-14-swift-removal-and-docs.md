# Plan rust-14-swift-removal-and-docs: Remove All Swift Artifacts, Synchronize Documentation, and Run Final Verification Gate

- **planId**: `rust-14-swift-removal-and-docs`
- **dependsOn**: `["rust-13-tooling-scripts-packaging"]`
- **writePaths**:
  - `Package.swift`
  - `Package.resolved`
  - `.swiftlint.yml`
  - `Sources`
  - `Tests`
  - `AGENTS.md`
  - `README.md`
  - `llms.txt`
  - `packaging/homebrew/README.md`
  - `target`
  - `frontend/dist`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `target`
  - `frontend/dist`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/command.md`

## Intent and Repository Context

Complete the migration to pure Rust + Tauri v2 by removing every remaining Swift file and SwiftPM manifest (`Package.swift`, `Package.resolved`, `.swiftlint.yml`, `Sources/`, `Tests/`), updating all repository documentation (`AGENTS.md`, `README.md`, `llms.txt`, `packaging/homebrew/README.md`) to describe the pure Rust + Tauri v2 workspace, and running the complete final verification gate across Rust, frontend, smoke suites, and macOS launch contracts.

## Non-Goals

- Do NOT modify `frontend/`, `Cargo.toml`, `Cargo.lock`, `crates/`, `src-tauri/`, `scripts/`, or `design-docs/` (already finalized in prior plans).
- Do NOT perform `git push` or create release tags.

## Target Files, Symbols, and Contracts

1. **Remove all Swift artifacts**:
   - Delete `Package.swift`, `Package.resolved`, `.swiftlint.yml`, `Sources/` (including `AppCore`, `AppCLI`, `CCUsageGaugeMenuBar`, `CPTY`, `CSQLite`, and `Sources/AppCore/Resources/Web`), and `Tests/` (`AppCoreTests`, `AppCLITests`).
   - Verify `find . -not -path './.git/*' -not -path './target/*' -not -path './frontend/node_modules/*' -name '*.swift'` returns completely empty!
2. **`AGENTS.md`**:
   - Preserve the entire `## Rule of the Responses`, `## Git Commit Policy`, `## Release Workflows`, and `## Design Documentation` sections verbatim!
   - Update `## Project Overview`, `## Development Environment`, `## Common Commands`, and replace `## Swift Code Development` with `## Rust and Tauri Code Development` (referencing Cargo workspace crates `crates/ccusage-core`, `crates/ccusage-cli`, `src-tauri`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, `mise run build|test|lint`, keeping files under 1000 lines).
3. **`README.md`**:
   - Update architecture overview, prerequisites (`rust`, `bun`, macOS Xcode CLT / Linux WebKitGTK only when building desktop), build/run/test/lint commands (`mise run ...`, `cargo ...`), native Rust incremental local usage scanning (noting local `ccusage` CLI is no longer required while remote SSH machines continue invoking remote `ccusage` over SSH), single-process Tauri v2 tray + dashboard (`dashboardAutostart` behavior and `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE` override for `ccusage-gauge-app`), Homebrew Formula & Cask installation (removing `ccusage` brew requirement for local-only installs), and Nix examples.
4. **`llms.txt`**:
   - Update repository map, crate layout (`crates/ccusage-core`, `crates/ccusage-cli`, `src-tauri`, `frontend`), build/test commands, and key specs to reflect pure Rust + Tauri v2 with zero Swift references.
5. **`packaging/homebrew/README.md`**:
   - Update Formula and Cask staging layouts (`bin/ccusage-gauge`, `bin/ccusage-gauge-app`, `share/ccusage-gauge/web`; `CCUsageGauge.app/Contents/MacOS/{ccusage-gauge-app,ccusage-gauge}` + `Contents/Resources/Web`, no nested helper app) and Cargo release build commands.

## Key Invariants and Pitfalls

- **Preserve `AGENTS.md` response & commit rules**: Never delete or weaken the response rules (English-only, no emojis, paraphrase) or git commit policy in `AGENTS.md`.
- **Zero Swift remnants**: Confirm zero `.swift` files remain anywhere in the working tree and `Package.swift`, `Package.resolved`, `.swiftlint.yml`, `Sources/`, `Tests/` are all gone.
- **Keep `frontend/` intact**: `git diff -- frontend/` must remain empty (except untracked build output in gitignored `frontend/dist`).

## Verification Commands and Acceptance Criteria

```bash
test -z "$(find . -not -path './.git/*' -not -path './target/*' -not -path './src-tauri/target/*' -not -path './frontend/node_modules/*' -name '*.swift')"
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --workspace
cargo build --locked --release -p ccusage-cli -p ccusage-gauge-app
(cd frontend && bun run check && bun test && bun run build)
scripts/smoke-isolated-runtime.sh
scripts/smoke-dashboard.sh --port 18181 --assets frontend/dist
scripts/smoke-packaged-assets.sh --layout all
scripts/smoke-packaged-assets.sh --layout all --expect-missing-diagnostics
scripts/build-homebrew-release.sh --dry-run
scripts/build-homebrew-cask-release.sh --dry-run
```

On macOS with GUI display access, additionally verify `DESIGN-LAUNCH-05` live launch contracts:
- Run `ccusage-gauge-app` with isolated config `dashboardAutostart: true` (confirms dashboard window opens on startup) and `dashboardAutostart: false` (confirms tray starts with window closed until opened), quitting cleanly between runs.
- Run `CCUSAGE_GAUGE_DESKTOP_EXECUTABLE="$PWD/target/debug/ccusage-gauge-app" cargo run --locked -p ccusage-cli -- dashboard` (confirms one tray-free window opens and foreground CLI exits `0` when the window closes; if headless/no WindowServer in sandbox, record as explicit environment-limited manual verification alongside the automated CLI/IPC tests).

Done when:
1. All automated verification commands above exit `0`.
2. Zero `.swift` files or SwiftPM files remain in the repository.
3. `AGENTS.md`, `README.md`, `llms.txt`, and `packaging/homebrew/README.md` accurately document the pure Rust + Tauri v2 workspace.

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized build artifact roots**:
  - `target` (added to `writePaths` and `artifactRoots`, gitignored via `/target/`) authorizes Cargo test, clippy, debug, and release build outputs consumed by the final verification gate and smoke scripts.
  - `frontend/dist` (added to `writePaths` and `artifactRoots`, gitignored via `dist/`) authorizes generated output from `(cd frontend && bun run build)` while keeping all tracked `frontend/` source files untouched.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; external OS `$TMPDIR` (`mktemp -d`) authorized for smoke scripts. Never stage or commit `target/` or `frontend/dist/`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
