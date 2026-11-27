# Plan rust-04-pricing-engine: Rust USD Pricing Engine (LiteLLM Cache, Fallback Table, Antigravity Schedule)

- **planId**: `rust-04-pricing-engine`
- **dependsOn**: `["rust-01-workspace-foundation"]`
- **writePaths**:
  - `crates/ccusage-core/src/pricing`
  - `crates/ccusage-core/tests/pricing_tests.rs`
  - `tmp/cargo-target/rust-04-pricing-engine`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `tmp/cargo-target/rust-04-pricing-engine`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`

## Intent and Repository Context

Implement the pure-Rust exact-decimal (`rust_decimal::Decimal`) USD pricing engine covering Claude Code, OpenAI/Codex, and Google Gemini/Antigravity models using cached LiteLLM pricing JSON (refreshed asynchronously at most once per 24 hours via an injectable HTTP transport), a built-in compiled fallback pricing table, exact Antigravity date-effective pricing transitions, 5m/1h cache-creation rates, and above-200k token tier pricing.

Existing Swift references to imitate:
- `Sources/AppCore/AntigravityUsage.swift:AntigravityModelPricing`: exact date-effective rates (`gemini-4-neon`, `gemini-4-hydrogen`, `gemini-4-argon` / `gemini-4-carbon` on `2026-12-01 UTC` = `1_796_083_200`, `gemini-3.8-flash` on `2027-01-01 UTC` = `1_798_761_600`).

## Non-Goals

- Do NOT perform live unmocked network calls in unit tests.
- Do NOT abort usage collection when LiteLLM refresh fails or times out.
- Do NOT modify `Cargo.toml`, `Cargo.lock`, `lib.rs`, or `types.rs`.

## Target Files, Symbols, and Contracts

1. **`crates/ccusage-core/src/pricing/mod.rs`**: re-exports `PricingEngine`, `ModelPricingRates`, `LiteLlmPricingCache`, `AntigravitySchedule`, `ReqwestPricingTransport`.
2. **`crates/ccusage-core/src/pricing/litellm_cache.rs`**:
   - Fetches `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json` into `~/.cache/ccusage-gauge/litellm-pricing.json` (`0600` inside `0700` dir, atomic write) at most once every 24 hours via `PricingHttpTransport` (10s timeout, non-blocking).
   - Failure/corrupt-response handling: retain last valid on-disk cache if present, otherwise use built-in fallback table; never fail or block snapshot collection.
   - Parse LiteLLM JSON entries (`input_cost_per_token`, `output_cost_per_token`, `cache_read_input_token_cost`, `cache_creation_input_token_cost`, `cache_creation_input_token_cost_above_1hr`, `input_cost_per_token_above_200k_tokens`, `output_cost_per_token_above_200k_tokens`, `cache_read_input_token_cost_above_200k_tokens`, `cache_creation_input_token_cost_above_200k_tokens`, tolerating missing/unknown fields and string/number decimals).
3. **`crates/ccusage-core/src/pricing/fallback_table.rs`**:
   - Compiled built-in pricing table covering current Anthropic Claude (Opus 4/4.1/4.5/4.6, Sonnet 4/4.5/4.6, Haiku 3.5/4.5, 3.7 Sonnet, etc.), OpenAI/Codex (gpt-5/5.1/5.2/5.3/5.4/codex, gpt-4.1, o3, o4-mini, etc.), and Google Gemini model families so first-run/offline environments produce accurate USD costs.
4. **`crates/ccusage-core/src/pricing/antigravity.rs`**:
   - Exact `AntigravityModelPricing` schedule matching `Sources/AppCore/AntigravityUsage.swift:161-192`:
     - `gemini-4-neon`: `$3` / `$15` / `$0.15` per 1M tokens (input / output / cache_read)
     - `gemini-4-hydrogen`: `$1.5` / `$7.5` / `$0.075`
     - `gemini-4-argon`, `gemini-4-carbon`: before `2026-12-01T00:00:00Z` -> `$2` / `$10` / `$0.1`, at/after -> `$4` / `$20` / `$0.2`
     - `gemini-3.8-flash`: before `2027-01-01T00:00:00Z` -> `$0.75` / `$3.75` / `$0.075`, at/after -> `$1.5` / `$7.5` / `$0.15`
     - Unknown Antigravity models fall through to LiteLLM / fallback or `$0` retaining token counts.
5. **`crates/ccusage-core/src/pricing/engine.rs`**:
   - `PricingEngine::price_event(&self, event: &TimestampedUsageEvent) -> PricedUsageEvent`.
   - Model resolution order:
     1. Antigravity date-effective schedule (when `event.agent == "agy"` and model matches scheduled family);
     2. Exact normalized lowercase key in LiteLLM table;
     3. Provider-prefixed variants (`anthropic/`, `openai/`, `google/`, `gemini/`);
     4. Stripped trailing date/snapshot suffixes (e.g., `-20250514`, `-2025-08-07`, `:0`);
     5. Built-in fallback table (same prefix/suffix resolution);
     6. Unknown model -> `Decimal::ZERO` cost while preserving all token counts.
   - Token-bucket costing (exact `Decimal`):
     - `unclassified_cache_creation = cache_creation_tokens.saturating_sub(cache_5m + cache_1h)`.
     - `effective_5m = cache_5m + unclassified_cache_creation`.
     - Fallback multipliers when LiteLLM omits explicit cache rates: `cache_5m_rate = 1.25 * input_rate`, `cache_1h_rate = 2.0 * input_rate`, `cache_read_rate = 0.1 * input_rate`.
     - Above-200k tiering: when LiteLLM defines `*_above_200k_tokens` rates and a token bucket exceeds `200_000` in a turn (or turn prompt tokens exceed `200_000` per LiteLLM/Anthropic semantics—apply tiered rate cleanly to tokens above `200_000` for each tiered bucket present), compute first `min(tokens, 200_000)` at base rate plus `tokens.saturating_sub(200_000)` at tiered rate.

## Key Invariants and Pitfalls

- **Exact `Decimal` math only**: Never compute token USD prices in `f64` and convert back; parse LiteLLM rates directly into `Decimal`.
- **24-hour refresh throttle**: Check file `mtime` (or cached timestamp) against injectable clock; do NOT re-fetch on every 20-second poll.
- **Permissions**: Write `litellm-pricing.json` with mode `0600` via atomic temp-file rename in `0700` cache dir.

## Test Cases (`crates/ccusage-core/tests/pricing_tests.rs`)

- LiteLLM cache refresh -> fetches once when missing/older than 24h, writes `0600` cache, skips network call within 24h.
- Offline / HTTP 500 / malformed JSON refresh failure -> falls back to prior valid on-disk cache if present, or built-in fallback table if absent, without returning an error.
- Model resolution -> exact key, provider-prefixed (`anthropic/claude-...`), date-suffix stripped (`claude-sonnet-4-20250514`), built-in fallback, and unknown model (`$0` cost, tokens preserved).
- Claude cache-creation costing -> explicit 5m + 1h + unclassified remainder priced at 5m rate, plus fallback `1.25x` / `2.0x` / `0.1x` multipliers when LiteLLM omits explicit cache rates.
- Above-200k tiered pricing -> turn with 250,000 input tokens prices 200,000 at base rate and 50,000 at above-200k rate with exact `Decimal` result.
- Antigravity date-effective schedule -> `gemini-4-argon` at `2026-11-30T23:59:59Z` vs `2026-12-01T00:00:00Z` switches from `$2/$10/$0.1` to `$4/$20/$0.2`; `gemini-3.8-flash` transitions on `2027-01-01T00:00:00Z`.

## Verification Commands and Acceptance Criteria

```bash
rustfmt --edition 2021 --check crates/ccusage-core/src/pricing/*.rs crates/ccusage-core/tests/pricing_tests.rs
CARGO_TARGET_DIR=tmp/cargo-target/rust-04-pricing-engine cargo test --locked -p ccusage-core --test pricing_tests
```

Done when both commands exit `0` and all pricing test cases pass with zero network access. (Crate-wide `cargo clippy` runs in serial Wave 3 join plan `rust-09`.)

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Authorized Cargo artifact root**: `tmp/cargo-target/rust-04-pricing-engine` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`) isolates this parallel Wave 2 plan's Cargo build output (`CARGO_TARGET_DIR=tmp/cargo-target/rust-04-pricing-engine`) with zero intra-wave path overlap or target lock contention.
- **Cargo cache & temp scratch authority**: Read/lock-only access to Wave 1's populated Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, or `CARGO_HOME=tmp/cargo-home` if `tmp/cargo-home` was created by `rust-01`) under `--locked` with zero new dependency downloads; OS `$TMPDIR` (`tempfile`) authorized for test fixtures. Never stage or commit `tmp/`.

## Amendment 2026-11-27 (blocked-wave-scope-or-contract, round 2)

- **Workspace edition on standalone `rustfmt` gate**: Standalone `rustfmt` defaults to Rust 2015 without a repository `rustfmt.toml` and rejects `async` syntax in `crates/ccusage-core/src/pricing/litellm_cache.rs` and `crates/ccusage-core/tests/pricing_tests.rs`. Amended the verification gate to `rustfmt --edition 2021 --check crates/ccusage-core/src/pricing/*.rs crates/ccusage-core/tests/pricing_tests.rs` matching `[workspace.package] edition = "2021"` in `Cargo.toml`, keeping Wave 2 plans parallel with zero extra root write paths.

## Progress Log

Record progress in a dedicated worker progress note; read each target file fresh before editing, capture pre/post content hashes to detect unexpected drift, never modify `impl-plans/active/` or run concurrent git operations, and keep all edits strictly within `writePaths`.

### Implementation progress (2026-11-27)

Implemented the pricing module, injectable asynchronous LiteLLM refresh and atomic cache, built-in model rates, decimal token buckets, and Antigravity effective-date schedules. Added offline integration tests in `crates/ccusage-core/tests/pricing_tests.rs`. Final formatting and locked test results are recorded in `tmp/rust-tauri-migration-20261126/rust-04-pricing-engine/`; independent integrity and adversarial reviews remain downstream.

Verification pending: the prescribed bare `rustfmt --check` parses as Rust 2015 and rejects async syntax; `rustfmt --edition 2021 --check` passes. The locked offline pricing test currently fails to compile sibling `remote/mutation_owner.rs` and `cache/aggregation_cache.rs` before running tests. Re-run both prescribed gates after the sibling compile failures and formatter edition configuration are resolved; no test acceptance is claimed.

### Verification rerun (2026-11-27)

The locked pricing integration tests now pass offline (3 passed, 0 failed); the previously reported sibling compilation failures did not recur. The exact bare `rustfmt --check` still fails because standalone rustfmt defaults to Rust 2015 and cannot parse the required async implementation; the same check with `--edition 2021` passes. The exact formatting acceptance remains blocked pending an authorized repository formatter edition configuration or an amended verification command. Logs are in `tmp/rust-tauri-migration-20261126/rust-04-pricing-engine/`. Formal reviews and workflow finalization remain downstream.

### Current verification (2026-11-27)

Offline locked pricing tests passed again (3 passed, 0 failed; `step6-cargo-final.log`). The exact standalone rustfmt gate failed on Rust 2015 parsing of async syntax (`step6-rustfmt-final2.log`); completion remains blocked until the command is amended to specify the workspace edition or an authorized formatter configuration is provided. Independent reviews and finalization remain downstream.

### Current shared-tree verification rerun (2026-11-27)

The locked offline pricing tests passed (3 passed, 0 failed; `step6-current-test.log`). The exact standalone `rustfmt --check` failed because it parses async syntax as Rust 2015 (`step6-current-rustfmt.log`); the same check with `--edition 2021` passed (`step6-current-rustfmt-edition.log`). The exact acceptance gate requires an amended command or an authorized formatter configuration covering both source and tests. Independent review and workflow finalization remain downstream.
