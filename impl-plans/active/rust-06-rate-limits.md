# Plan rust-06-rate-limits: Subscription Rate-Limit Service (Claude, Codex, Gemini PTY)

- **planId**: `rust-06-rate-limits`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/rate_limits`
  - `crates/ccusage-core/tests/rate_limits_tests.rs`
  - `tmp/cargo-target/rust-06-rate-limits`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-06-rate-limits`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/design-subscription-rate-limits.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement `RateLimitUsageService` in `ccusage-core` with full parity for all three local subscription providers (Claude Code, Codex, Gemini via `agy` POSIX PTY), per-provider throttling, in-flight coalescing, read-only non-refreshing credential handling, observation persistence (`~/.local/ccusage-gauge/rate-limits.json`), and compact menu formatting.

Existing Swift references to imitate:
- `Sources/AppCore/RateLimitUsageService.swift`: per-provider throttling (`claude`: 300s, `codex`/`gemini`: 60s), in-flight coalescing, observation cache fallback.
- `Sources/AppCore/RateLimitUsageClaude.swift` & `RateLimitUsageClaudeCredentials.swift`: Keychain lookup (`/usr/bin/security find-generic-password -s "Claude Code-credentials" -w`, 10s timeout, stderr discarded) with `<claude_home>/.credentials.json` fallback, `GET https://api.anthropic.com/api/oauth/usage` with redirects disabled and 15s timeout, redacting token wrapper.
- `Sources/AppCore/RateLimitUsageCodex.swift` & `RateLimitUsageCodexSessionFallback.swift`: `<codex_home>/auth.json`, `GET https://chatgpt.com/backend-api/wham/usage` (redirects disabled, 15s timeout), offline reverse-scan fallback over newest 10 `<codex_home>/sessions/**/*.jsonl` files for `token_count` `rate_limits`, window validation.
- `Sources/AppCore/RateLimitUsageGemini.swift`: `agy` binary discovery (`PATH` + `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`), `0700` temp dir, POSIX PTY (`portable-pty`, 36x120, `TERM=xterm-256color`), polling `status.json` up to 12s deadline, Ctrl-D (`0x04`) graceful exit + `SIGKILL` + wait, parsing `product == "antigravity"`, `gemini-5h`, `gemini-weekly`, and `status.json` `plan_tier` into `RateLimitObservation.plan_type`.
- `Sources/AppCore/RateLimitObservationFileStore.swift` & `RateLimitDisplayFormatting.swift`: atomic `0600` observation JSON map and compact menu/tooltip line formatting.

## Non-Goals

- Never refresh OAuth tokens, log tokens, persist credentials, or return credentials in any error or API response.
- Do NOT make live network/Keychain calls in unit tests (use injectable transports, credential readers, and fake `agy` PTY fixture scripts).
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/rate_limits/mod.rs`**: re-exports `service`, `store`, `formatting`, `claude`, `codex`, `gemini`, `transport`.
2. **`crates/ccusage-core/src/rate_limits/transport.rs`**:
   - Injectable `RateLimitHttpTransport` trait + production `ReqwestRateLimitTransport` configured with `redirect::Policy::none()` and 15s timeout.
   - `RedactedSecret(String)` wrapper whose `Debug` and `Display` always output `"[REDACTED]"`.
3. **`crates/ccusage-core/src/rate_limits/claude.rs`**:
   - Reads macOS Keychain via fixed-arg `Command::new("/usr/bin/security").args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])` (10s timeout, stderr null, stdout in memory only) with fallback to `<claude_home>/.credentials.json` (`CLAUDE_CONFIG_DIR` or `$HOME/.claude`).
   - Parses OAuth response (`five_hour`, `seven_day`, extended-usage `extra_usage` credit fields) into `RateLimitObservation`.
4. **`crates/ccusage-core/src/rate_limits/codex.rs`**:
   - Reads `<codex_home>/auth.json` (`CODEX_HOME` or `$HOME/.codex`); issues `GET https://chatgpt.com/backend-api/wham/usage`; validates window duration/reset invariants matching `RateLimitUsageCodex.swift`.
   - Offline fallback: reverse-scans up to newest 10 `<codex_home>/sessions/**/*.jsonl` files for `token_count` `rate_limits` + credit lines.
5. **`crates/ccusage-core/src/rate_limits/gemini.rs`**:
   - Resolves `agy` in `PATH` plus `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`.
   - Creates `0700` temp directory, spawns `agy --settings '{"statusLine":{"type":"command","command":"..."}}'` (matching `RateLimitUsageGemini.swift`) attached to a 36x120 POSIX PTY via `portable-pty` (`TERM=xterm-256color`), drains PTY reader on a helper thread/task while polling `status.json` up to 12s deadline, sends `0x04` (Ctrl-D) for bounded graceful exit before `SIGKILL` + `wait()` so zero zombie/orphan child processes remain.
   - Parses `product == "antigravity"`, `gemini-5h` (`18_000s`), `gemini-weekly` (`604_800s`), and `status.json` `plan_tier` into `RateLimitObservation { plan_type, observed_at, windows, credits: None }`.
6. **`crates/ccusage-core/src/rate_limits/store.rs` & `formatting.rs`**:
   - `RateLimitObservationFileStore`: atomic `0600` JSON persistence at `~/.local/ccusage-gauge/rate-limits.json` keyed by `claude`, `codex`, `gemini`.
   - `RateLimitDisplayFormatting`: `compact_menu_line`, ratio label text (`45%`, `--`), and tooltip credit lines (`Claude Code credit`, `Codex credit`).
7. **`crates/ccusage-core/src/rate_limits/service.rs`**:
   - `RateLimitUsageService`: async actor/Arc state with per-provider throttle windows (`claude`: 300s, `codex`: 60s, `gemini`: 60s), in-flight request coalescing (concurrent callers share one fetch), stale-observation preservation on failure, and `production(paths)` constructor.

## Key Invariants and Pitfalls

- **Redirects disabled**: Never follow HTTP 30x redirects on OAuth usage calls (prevents bearer-token leakage to redirected hosts).
- **PTY process cleanup**: Always drain PTY output (preventing write-buffer deadlock), enforce 12s deadline, send Ctrl-D, and escalate to `SIGKILL` + reap child + remove `0700` temp directory even on cancellation/timeout.
- **No snapshot dependency**: `/api/rate-limits` and tray rate-limit updates run independently of usage snapshot state.

## Test Cases (`crates/ccusage-core/tests/rate_limits_tests.rs`)

- `RedactedSecret` `Debug`/`Display` -> never contains raw token substring.
- Claude fetcher with injected credential + mock HTTP response (including `extra_usage` credit) -> decodes 5h/weekly windows accurately; HTTP 302 redirect -> rejected without following.
- Codex fetcher -> live API parse, invalid-window rejection, and offline fallback reading newest 10 rollout JSONL files in reverse order.
- Gemini `agy` PTY fetcher -> fake executable fixture script writing `status.json` inside the `0700` temp dir -> decodes `gemini-5h`, `gemini-weekly`, and `plan_tier` into `RateLimitObservation.plan_type`, exits cleanly with temp dir removed; hanging fixture script -> terminates within short test timeout via Ctrl-D / `SIGKILL` with no leaked child.
- `RateLimitUsageService` -> concurrent callers coalesce into 1 upstream fetch; calls within throttle window (`300s` Claude / `60s` Codex & Gemini) reuse cached observation; upstream failure retains previous observation.
- `RateLimitObservationFileStore` -> round-trips `claude`/`codex`/`gemini` map with `0600` permissions; `RateLimitDisplayFormatting::compact_menu_line` matches expected format.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check crates/ccusage-core/src/rate_limits/*.rs crates/ccusage-core/tests/rate_limits_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-06-rate-limits cargo test --locked -p ccusage-core --test rate_limits_tests
```

Done when both commands exit `0` and all rate-limit, PTY, redaction, throttling, and formatting tests pass. (Crate-wide `cargo clippy` runs in serial Wave 4 join plan `rust-10`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-06-rate-limits` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 2 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-06-rate-limits`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
