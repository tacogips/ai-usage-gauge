# Plan rust-03-incremental-scanner: Native Byte-Offset JSONL Scanner for Claude, Codex, and Antigravity

- **planId**: `rust-03-incremental-scanner`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/scanner`
  - `crates/ccusage-core/tests/scanner_tests.rs`
- **sharedPaths**: `[]`
- **artifactRoots**: `[]`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement the native Rust incremental byte-offset JSONL scanner for Claude Code, Codex, and Antigravity with per-file `(file_size, modified, scanned_through, boundary)` tracking, `reuse`/`resume`/`rescan` step selection, bounded reverse-scan optimization, pinned single-day active-boundary scan slot support, and read-only Antigravity workspace lookup. Zero external `ccusage` subprocesses and zero hard-link mirroring.

Existing Swift references to imitate:
- `Sources/AppCore/UsageEventFileScanCache.swift`: `UsageEventFileScan<Context>`, `UsageEventFileScanPlanner`, `UsageEventScanCachePolicy`, 256-byte boundary check, `reuse`/`resume`/`rescan`.
- `Sources/AppCore/UsageEventLogReader.swift`: JSONL discovery with `mtime` pre-filter, forward byte-range reader, reverse chunked line reader (`forEachLineFromEnd`), raw timestamp day pre-filter.
- `Sources/AppCore/ClaudeUsageEvents.swift`: Claude pre-filter, JSON decoding, `sessionId\x1frequestId\x1fmessageId` identity deduplication keeping highest timestamp, `cwd`, `effort`/`perTurnEffort`, 5m/1h cache creation tokens.
- `Sources/AppCore/CodexUsageEvents.swift`: `CodexScanContext`, `CodexForwardParser`, line-1 `session_meta` probe, reverse-scan `pending` vs `newest_model`/`newest_effort` separation, `total_token_usage` watermark identity, `input = max(0, last.input - last.cached)`.
- `Sources/AppCore/AntigravityUsage.swift`: `brain/<uuid>/.system_generated/logs/{transcript.jsonl,transcript_full.jsonl}`, `step_index` keying, trailing effort suffix split, read-only `conversation_summaries.db` lookup with `?immutable=1` only when no `-wal` exists.

## Non-Goals

- Do NOT compute USD pricing, 5-hour billing blocks, or write to `UsageAggregationCache` (handled in `rust-04`, `rust-05`, `rust-09`).
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/scanner/mod.rs`**: re-exports `file_scan`, `log_reader`, `claude`, `codex`, `antigravity`, and `LocalUsageScanner` facade (including scan read-metrics instrumentation hooks for testing zero re-reads of unchanged files and tail-only resume reads!).
2. **`crates/ccusage-core/src/scanner/file_scan.rs`**:
   - `UsageEventFileScan<Context>` (`canonical_path`, `file_size`, `modified`, `scanned_through`, `boundary: Vec<u8>` up to 256 bytes, `since: String`, `timezone_id: String`, `events: HashMap<String, CachedUsageEvent>`, `context: Context`).
   - `ScanStep::Reuse`, `ScanStep::Resume`, `ScanStep::Rescan` with exact transition predicates (`design-rust-tauri-migration.md:95-97`).
   - `ScanCacheRetention`: manages both the normal rolling cache `[retention_floor, today]` AND an optional bounded single-day **active-boundary scan slot** pinned to `start_day < retention_floor` (storing only events falling on `start_day`, evictable immediately when `start_day` advances or cycle changes, never populated by one-off historical queries).
   - Expose per-file `ScanFileTransition` metadata (`Reuse`, `Resume { prev_offset, new_offset, new_events }`, `Rescan`) so `BlockAnchor` (`rust-09`) can verify validated recent-only appends without re-reading historical prefixes!
3. **`crates/ccusage-core/src/scanner/log_reader.rs`**:
   - Canonicalized JSONL recursive walker with `mtime >= modified_since` pre-filter; skips missing/non-directory roots without error.
   - Forward byte-range line reader (`for_each_line_range(path, start, end, needles)`) stopping cleanly at the last complete `\n` (never advancing `scanned_through` past an incomplete trailing line!).
   - Reverse chunked line reader (`for_each_line_from_end(path, end, needles)`) with fast timestamp-day pre-decode extractor.
4. **`crates/ccusage-core/src/scanner/claude.rs`**:
   - `ClaudeUsageEventLoader`: pre-filters `"type":"assistant"`, decodes assistant usage lines, builds identity `"{session_id}\x1f{request_id}\x1f{message_id}"` (fallback `requestId` -> `message.id`), retains highest-timestamp entry per identity, preserves opaque `cwd` (empty/whitespace -> `None`) and normalized `effort`/`perTurnEffort`, extracts `cache_creation.ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens`.
5. **`crates/ccusage-core/src/scanner/codex.rs`**:
   - `CodexScanContext { pub session_id: String, pub model: String, pub directory: Option<String>, pub effort: Option<String> }`.
   - `CodexUsageEventLoader`: pre-filters `"type":"session_meta"`, `"type":"turn_context"`, `"type":"token_count"`; extracts `last_token_usage` and `total_token_usage` watermark `"{input}:{cached}:{output}:{reasoning}:{total}"`; sets `input_tokens = last.input.saturating_sub(last.cached)`, `output_tokens = last.output`, `cache_read_tokens = last.cached`, `cache_creation_tokens = 0`.
   - **Exact `DESIGN-CODEX-04` reverse-scan state machine**: probe line 1 (up to one reverse-chunk size) for `session_meta`; maintain `pending` in-range (`day >= since`) `token_count` events separately from `newest_model` / `newest_effort` (captured once from the first `turn_context` seen in reverse). NEVER stop or set `stop_requested` while `pending` is non-empty—keep scanning past pre-cutoff `token_count` events (`day < since`) even when `newest_model` is already set until preceding `turn_context` lines (including before `since`) resolve all `pending` events into `resolved`. Set `stop_requested` only when `pending` is empty and past cutoff (`day < since` or `raw_day < scan_floor` with `pending` and `resolved` empty), stopping once `newest_model` is known. Flush `resolved` on `session_meta` and at scan end; save `(session_id, directory, model: newest_model, effort: newest_effort)` in `CodexScanContext` for forward `resume`.
6. **`crates/ccusage-core/src/scanner/antigravity.rs`**:
   - `AntigravityUsageLoader`: scans `<root>/brain/<uuid>/.system_generated/logs/`, prefers `transcript.jsonl` over `transcript_full.jsonl`, uses incremental byte-offset `UsageEventFileScan<AntigravityContext>` keyed by `step_index`, decodes `source == "MODEL"`, `created_at`, `model`, token counts (`0..=i32::MAX`, at least one `> 0`), splits trailing effort suffix (`-xhigh`, `-high`, `-medium`, `-low`, `-max`, `-minimal`), and reads workspace directory from `conversation_summaries.db` read-only (`SQLITE_OPEN_READ_ONLY`, falling back to `?immutable=1` URI ONLY when `<db>-wal` does not exist; never creates sidecars).

## Key Invariants and Pitfalls

- **Incomplete trailing line safety**: Only advance `scanned_through` to the byte immediately after the last complete `\n` so an in-progress JSONL write is picked up cleanly on the next poll without skipping or corrupting a line.
- **Boundary mismatch / truncation / timezone change**: Any of these MUST trigger a full `Rescan`, never a partial `Resume`.
- **Opaque `cwd`**: Never canonicalize, symlink-resolve, tilde-expand, or stat `cwd` from JSONL events; retain exact non-empty string or `None`.
- **Antigravity read-only DB**: Never open `conversation_summaries.db` read-write and never pass `?immutable=1` while `-wal` exists.
- **Instrument byte reads**: Expose a lightweight `ScanStats { files_reused, files_resumed, files_rescanned, bytes_read }` counter so tests mechanically prove zero bytes read on `reuse` and tail-only bytes read on `resume`.

## Test Cases (`crates/ccusage-core/tests/scanner_tests.rs`)

- Unchanged file across two polls -> `files_reused == 1`, `bytes_read == 0`.
- Appended complete line -> `files_resumed == 1`, reads only appended bytes, counts appended event once; incomplete trailing line without `\n` -> ignored until `\n` arrives, then decoded cleanly.
- File truncated or rewritten in place (same/larger size with modified prefix failing 256-byte boundary check) -> triggers `Rescan` and replaces events accurately.
- Timezone change -> triggers `Rescan` and re-buckets local days accurately.
- Claude deduplication -> two lines sharing `(sessionId, requestId, message.id)` retain highest timestamp; extracts 5m/1h cache creation tokens, `cwd`, and `effort`/`perTurnEffort`.
- **Codex `DESIGN-CODEX-04` multi-turn pre-`since` fixture** -> rollout with `session_meta`, model A/`high` before `since`, pre-cutoff `token_count`, in-range `token_count` for A, model B/`low` in-range, in-range `token_count` for B -> bounded reverse scan matches full forward scan identically (event A has A/`high`, event B has B/`low`, both have `session_meta` dir), and subsequent appended event on `resume` inherits B/`low`.
- Antigravity -> prefers `transcript.jsonl` over `transcript_full.jsonl`, deduplicates by `step_index`, splits trailing `-high`/`-xhigh` effort suffixes, resolves workspace from read-only `conversation_summaries.db` without creating `-wal`/`-shm` sidecars, and skips `?immutable=1` when `-wal` exists.
- Pinned single-day active-boundary slot -> retains only `start_day` events, hits `reuse` (`bytes_read == 0`) on consecutive polls, evicts cleanly on advance/clear.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --check crates/ccusage-core/src/scanner/*.rs crates/ccusage-core/tests/scanner_tests.rs
cargo test --locked -p ccusage-core --test scanner_tests
```

Done when both commands exit `0` and all test cases above pass deterministically. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09`.)

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.
