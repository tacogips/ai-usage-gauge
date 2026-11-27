# Plan rust-07-remote-ssh-registry: Machine Registry v1-v3, Session Source Plans, SSH Runner, and Remote CCUsage Decoders

- **planId**: `rust-07-remote-ssh-registry`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/remote`
  - `crates/ccusage-core/tests/remote_ssh_tests.rs`
  - `tmp/cargo-target/rust-07-remote-ssh-registry`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-07-remote-ssh-registry`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/design-remote-machine-collection.md`
  - `design-docs/specs/design-machine-session-source-directories.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement the persisted machine registry (v3 schema + atomic v1/v2 migration + fail-closed validation), `MachineSessionSourcePlan` (used by both local native scanning and remote SSH collection), serialized `MachineRegistryMutationOwner`, `SSHCCUsageCommandRunner` (direct/jump/command structured proxy adapters + fixed positional remote adapter), `MachineDiagnosticClassifier`, and tolerant `CCUsageClient` JSON decoders for remote `ccusage` v20.0.17+ (`blocks`, `daily` with `--by-agent` fallback, `session`).

Existing Swift references to imitate:
- `Sources/AppCore/Machines.swift`: `MachineRegistryStore`, `MachineValidation`, closed v1/v2->v3 migration, `0700`/`0600` fail-closed safety checks, SSH option allowlist, host/port/user/identity/proxy validation.
- `Sources/AppCore/MachineSessionSourcePlan.swift`: `MachineSessionSourcePlan`, env defaults (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`), `~` expansion, lexical/symlink canonicalization, shallowest-first containment deduplication, deterministic SHA-256 `fingerprint`.
- `Sources/AppCore/MachineRegistryTransaction.swift`: `MachineRegistryMutationOwner`, serialized CRUD, disk+runtime+metadata compensation rollback, `registry_reconciliation_required` latch.
- `Sources/AppCore/CCUsageCommandRunner.swift`, `CCUsageProcessRunner.swift`, `CCUsageMultiSourceCoordinator.swift`: `SSHCCUsageCommandRunner`, fixed POSIX positional adapter, remote `mtime` cutoff token, retries, exit-255 transport vs 1..254 remote-command failure classification.
- `Sources/AppCore/MachineDiagnostics.swift`: closed ordered diagnostic classification (`host_key`, `authentication`, `proxy_unreachable`, `timeout`, `remote_command`, `invalid_response`, `cache_failed`, `internal_error`) with sanitized messages/remediations.
- `Sources/AppCore/CCUsage.swift` & `MultiSourceUsageCoalescing.swift`: remote `ccusage` `blocks --json`, `daily --json` (20.1+ + cached `--by-agent` 20.0.17 fallback), `session --json` decoding without double-counting model aggregates.

## Non-Goals

- Do NOT implement local `ccusage` subprocess execution or `SessionScanPruning` hard-link mirroring (explicitly superseded by native Rust scanning in `rust-03`/`rust-09`).
- Do NOT add remote JSONL transport or shell interpolation of user inputs.
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/remote/mod.rs`**: re-exports `registry`, `source_plan`, `mutation_owner`, `ssh_runner`, `diagnostics`, `ccusage_client`.
2. **`crates/ccusage-core/src/remote/registry.rs`**:
   - `MachineRegistryStore::new(file_path: PathBuf)`: fail-closed `load()` (absent file = empty SSH list + default `localSessionSources`; unsafe dir/file ownership/type/symlink/hardlink/permissions, unknown/duplicate JSON keys, invalid version, or invalid descriptors fail closed without quarantine or fallback).
   - Atomic v1 -> v3 and v2 -> v3 migration before publication; migration failure preserves original bytes verbatim.
   - `MachineValidation`: validates canonical slugs (`local` reserved), indexed `codexSessionDirs[i]` / `claudeConfigDirs[i]` (1..=4096 UTF-8 bytes, starts with `/` or `~` or `~/`, no ASCII control chars, no `~user`), SSH host/port/user/identity path/remote executable, closed SSH option allowlist (rejecting raw `-J`, `ProxyJump`, `ProxyCommand`, forwarding, hooks, config overrides, inline private keys), and structured `jump` / `command` proxy adapters.
3. **`crates/ccusage-core/src/remote/source_plan.rs`**:
   - `MachineSessionSourcePlan::resolve_local(descriptor, env)` and `MachineSessionSourcePlan::from_remote_probe(...)`.
   - Implements exact 6-step planning order from `design-machine-session-source-directories.md:68-97`: default root if enabled (`CODEX_HOME` / `CLAUDE_CONFIG_DIR` presence wins untouched, else `$HOME/.codex` / `$HOME/.claude`), explicit roots in order, `~` expansion + canonicalization for explicit roots, append `sessions/` or `projects/`, shallowest-first containment dedup, skip missing/non-directory scopes.
   - Deterministic `fingerprint()` over source-semantics version, agent kind, include-default flags, normalized configured roots, and resolved plan including missing/non-directory state.
4. **`crates/ccusage-core/src/remote/mutation_owner.rs`**:
   - `MachineRegistryMutationOwner<R: MachineRuntimeReconciler, D: DirectoryNameLifecycle>`: serialized `tokio::sync::Mutex` CRUD (`create`, `replace`, `patch` including `PATCH local` for the 4 source fields only, `delete`), atomic `0600` save, `DirectoryNameLifecycle` prepared->committed/rollback transitions, runtime reconciliation with rollback on failure, `registry_reconciliation_required` latch when compensation fails, and `metadata_cleanup_pending_machine_ids` tracking.
5. **`crates/ccusage-core/src/remote/ssh_runner.rs`**:
   - `SSHCCUsageCommandRunner` + `RetryingSSHRunner` + injectable process-execution trait for deterministic unit tests.
   - Fixed argument arrays (`-F /dev/null`, `-o BatchMode=yes`, strict host-key verification, allowlisted options only, structured `jump` or `command` proxy adapter with zero raw shell interpolation, POSIX-quoted remote positional adapter with per-agent `CODEX_HOME`/`CLAUDE_CONFIG_DIR` isolation and remote `mtime` cutoff token).
   - Bounded retries on transport failures only, timeout enforcement with process kill + reap.
6. **`crates/ccusage-core/src/remote/diagnostics.rs`**:
   - `MachineDiagnosticClassifier::classify(&err) -> MachineDiagnostic` with closed ordered error codes and sanitized human messages + safe remediation (never leaking stderr, env, paths, hosts, users, or keys).
7. **`crates/ccusage-core/src/remote/ccusage_client.rs`**:
   - `RemoteCCUsageClient`: decodes remote `blocks --json`, `daily --json` (accepting 20.1+ shape first, falling back to and caching `--by-agent` for 20.0.17), and `session --json`; sums model-level costs exactly once (never double-counting top-level totals); coalesces multi-source remote outputs deterministically and stamps `machine` id after merging.

## Key Invariants and Pitfalls

- **Duplicate JSON key rejection**: Standard `serde_json` overwrites duplicate keys silently; scan raw JSON tokens or use a duplicate-key-detecting deserializer for `machines.json` strict closed-document checks.
- **No shell interpolation**: Never pass user-supplied strings to `/bin/sh -c` locally; remote tokens must be strictly single-quoted POSIX literals passed to the fixed positional adapter.
- **Immutability of `local`**: `PUT` and `DELETE` on `local` return conflict (`409`); `PATCH local` accepts only the four source fields.

## Test Cases (`crates/ccusage-core/tests/remote_ssh_tests.rs`)

- Registry load on absent file -> empty SSH list + default `localSessionSources`; unsafe permissions/symlink/unknown field/duplicate key -> fail closed.
- Exact v1 and v2 registry documents -> atomically migrated to v3; malformed migration target -> preserves original bytes untouched.
- `MachineValidation` -> rejects `~other/dir`, relative paths, control chars, raw `ProxyCommand`/`-J` in `extraOptions`, inline `-----BEGIN` key material, and reports indexed keys `codexSessionDirs[0]`.
- `MachineSessionSourcePlan` -> default opt-out, env presence wins, symlink/nested containment deduplication (shallowest wins), missing root skipped without error, deterministic fingerprint changes on missing-to-present transition.
- `MachineRegistryMutationOwner` -> serialized create/replace/patch/delete, `PATCH local` source update, rollback on runtime failure restoring disk & directory-name markers, and `registry_reconciliation_required` latch on double failure.
- `SSHCCUsageCommandRunner` -> verifies generated argv arrays for direct, `jump`, and `command` adapters (no raw shell, `-F /dev/null`, strict host-key verification, per-agent isolation); classifies exit 255 vs 1..254 vs timeout vs host-key vs auth into sanitized diagnostics without leaking raw stderr.
- `RemoteCCUsageClient` -> decodes sanitized v20.0.17 (`--by-agent` fallback cached) and v20.1+ fixtures for `blocks`, `daily`, and `session` without double counting.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --edition 2021 --check crates/ccusage-core/src/remote/*.rs crates/ccusage-core/tests/remote_ssh_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-07-remote-ssh-registry cargo test --locked -p ccusage-core --test remote_ssh_tests
```

Done when both commands exit `0` and all registry, source-plan, mutation-owner, SSH argv/diagnostic, and remote decoder tests pass. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-07-remote-ssh-registry` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 2 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-07-remote-ssh-registry`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Amendment 2026-11-27 (blocked-wave-scope-or-contract, round 2)

- **Workspace edition on standalone `rustfmt` gate**: Standalone `rustfmt` defaults to Rust 2015 without a repository `rustfmt.toml` and rejects `async` functions in `crates/ccusage-core/src/remote/mutation_owner.rs`. Amended the verification gate to `rustfmt --edition 2021 --check crates/ccusage-core/src/remote/*.rs crates/ccusage-core/tests/remote_ssh_tests.rs` matching `[workspace.package] edition = "2021"` in `Cargo.toml`, keeping Wave 2 plans parallel without authorizing a root `rustfmt.toml`.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.

### Implementation progress (2026-11-27)

- Added plan-owned registry, source-plan, mutation-owner, SSH runner, diagnostics, and remote decoder modules, plus integration tests. The isolated `cargo test --locked -p ccusage-core --test remote_ssh_tests` passed 7 tests (log: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/test-final.log`).
- The exact specified standalone `rustfmt --check` fails because rustfmt assumes Rust 2015; `rustfmt --edition 2021 --check` passes. Resolving the exact gate requires an authorized rustfmt configuration or a corrected verification command. Logs are in the plan evidence directory.
- Completion remains open: remote source probing and comprehensive tests for SSH adapters, registry safety and migrations, mutation serialization and compensation, and decoder compatibility require further implementation or verification. Formal review remains downstream.

### Continuation assessment (2026-11-27)

- The exact isolated Cargo test passed 7 tests; the exact standalone rustfmt gate failed because it parses async Rust as edition 2015. Logs: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/test-current.log` and `rustfmt-current.log` in the same directory. A checkpoint amendment must authorize an applicable rustfmt configuration outside this plan or correct the gate to specify the workspace edition.
- Remaining implementation and verification include the fixed remote path probe, normalized source fingerprints, partial local PATCH under the mutation lock, deletion compensation when disk restoration fails, deterministic coalescing, and broader registry, SSH and decoder tests. Review, commit and publication remain downstream.

### Step 6 verification and blocker (2026-11-27)

- Current isolated Cargo integration test passed 7 tests with offline dependency access; complete log: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/test-step6-final.log`.
- The exact required standalone rustfmt command still exits 1 because it defaults to Rust 2015 and cannot parse async functions; complete log: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/rustfmt-step6-final.log`. A checkpoint amendment must correct the command to specify the workspace edition or authorize an applicable configuration outside the current write scope.
- Implementation remains incomplete: remote probing, normalized fingerprints, partial local PATCH, deletion compensation, deterministic coalescing, and the broader required behavioral tests remain outstanding. Formal review and workflow finalization are downstream.

### Redispatch progress (2026-11-27)

- Normalized configured source paths in fingerprints, added partial local source mutation under the owner lock, made coalesced output ordering deterministic, and prevented cancellation of deletion metadata when persistence rollback fails. Added three regression tests for fingerprints, partial local updates, and point ordering.
- The amended edition-2021 rustfmt gate passed; the isolated locked remote integration suite passed 10 tests. Complete logs: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/redispatch-20261127-rustfmt.log` and `redispatch-20261127-test.log` in the same directory.
- Completion remains open: the fixed SSH source probe execution and decoding are absent, and the assigned registry, mutation compensation, SSH adapters, and decoder behavior still require comprehensive regression coverage. Formal review and workflow finalization remain downstream.

### Implementation continuation 1 (2026-11-27)

- Implemented the fixed positional SSH source probe, strict NUL-delimited decoding, captured execution values, unavailable-source state, and empty-plan shortcut. Added bounded transport-only retries for sourced invocations and probes. Aligned runner option handling with registry validation.
- Added integration coverage for probe execution and malformed output, source aliases and environment precedence, registry link safety and migration failure, serialized local mutations, failed deletion compensation, jump arguments, sanitized diagnostics, source retries, and subprocess exit/timeout classification.
- The edition-2021 rustfmt gate and isolated locked remote integration suite passed; complete logs are `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/continuation1-final2-rustfmt.log` and `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/continuation1-final2-test.log`. The suite ran 19 tests with zero failures. These checks ran on the shared tree; formal reviews and workflow finalization remain downstream.

### Test-integrity revisions (2026-11-27)

- R07-TI-001: reused registry file-safety checks immediately before SSH launch and enforced registry connection validation in argv construction. Added direct, jump, command, per-agent isolation, and unsafe-file regression checks.
- R07-TI-002: added CRUD, local conflict, recoverable deletion rollback, and metadata cleanup retry checks. R07-TI-003: added concrete decoder, coalescing, provenance, and empty daily fallback checks. The added tests live in `crates/ccusage-core/src/remote/integration_regressions.rs` and are included by `remote_ssh_tests.rs` to keep Rust files under 1000 lines.
- Current scoped gates passed on the shared tree: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/ti-final-rustfmt.log` and `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/ti-final-test.log` (26 tests, zero failures). Independent review and workflow finalization remain downstream.

### Adversarial review revisions (2026-11-27)

- R07-ADV-001 and 002: reject null known-hosts destinations, percent placeholders in SSH paths, and relative multi-component remote executable paths at registry and runner boundaries.
- R07-ADV-003: fail all latched mutations before validation or closures and report the triggering save-plus-cancellation failure correctly. R07-ADV-004: normalize bounded diagnostics and restore the specified matching phrases and public text.
- Scoped verification passed on the shared tree: `tmp/rust-tauri-migration-20261126/rust-07-remote-ssh-registry/adv-final-rustfmt.log`, `adv-final-test.log` (30 tests), `adv-final-offline-test.log` (30 tests), and `adv-final-lib.log` (5 tests), all in the same evidence directory. Independent re-review remains downstream.

### Integration review repairs (2026-11-27)

- Replaced shadow lifecycle and runtime traits with the pinned shared contracts; serialized asynchronous reconciliation now carries monotonic revisions and preserves deletion metadata during compensation. Added real-store regression coverage and resolved scoped SSH probe Clippy diagnostics.
- Final scoped verification and its outcomes are recorded in the plan-local integration repair logs. Independent integration review and workflow finalization remain downstream.
