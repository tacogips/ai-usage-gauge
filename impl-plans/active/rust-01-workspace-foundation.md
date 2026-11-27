# Plan rust-01-workspace-foundation: Cargo Workspace, Shared Domain Contracts, and Path Resolution

- **planId**: `rust-01-workspace-foundation`
- **dependsOn**: `[]`
- **writePaths**:
  - `.gitignore`
  - `Cargo.toml`
  - `Cargo.lock`
  - `crates/ccusage-core/Cargo.toml`
  - `crates/ccusage-core/src/lib.rs`
  - `crates/ccusage-core/src/version.rs`
  - `crates/ccusage-core/src/paths.rs`
  - `crates/ccusage-core/src/types.rs`
  - `crates/ccusage-core/src/config/mod.rs`
  - `crates/ccusage-core/src/state/mod.rs`
  - `crates/ccusage-core/src/scanner/mod.rs`
  - `crates/ccusage-core/src/pricing/mod.rs`
  - `crates/ccusage-core/src/cache/mod.rs`
  - `crates/ccusage-core/src/aggregation/mod.rs`
  - `crates/ccusage-core/src/rate_limits/mod.rs`
  - `crates/ccusage-core/src/remote/mod.rs`
  - `crates/ccusage-core/src/dashboard/mod.rs`
  - `crates/ccusage-cli/Cargo.toml`
  - `crates/ccusage-cli/src/main.rs`
  - `src-tauri/Cargo.toml`
  - `src-tauri/Cargo.lock`
  - `src-tauri/src/lib.rs`
  - `src-tauri/src/tray_icons.rs`
  - `crates/ccusage-core/tests/paths_types_tests.rs`
  - `target`
  - `tmp/cargo-home`
- **sharedPaths**: `[]`
- **artifactRoots**:
  - `target`
  - `tmp/cargo-home`
- **designReferences**:
  - `design-docs/specs/design-rust-tauri-migration.md`
  - `design-docs/specs/architecture.md`
  - `design-docs/specs/command.md`

## Intent and Repository Context

Establish the single root Cargo workspace (`crates/ccusage-core`, `crates/ccusage-cli`, `src-tauri`) with one locked `Cargo.lock` and all shared domain data structures, traits, error types, version constant, and `AppPaths` environment-override resolution pinned up front. This unblocks seven parallel Wave 2 implementation plans without any worker touching `Cargo.toml`, `Cargo.lock`, or `crates/ccusage-core/src/lib.rs`.

Existing Swift references to imitate:
- `Sources/AppCore/Version.swift`: package version (`VERSION` file is `0.5.1`).
- `Sources/AppCore/Configuration.swift:AppPaths`: production path resolution and env overrides (`CCUSAGE_GAUGE_HOME`, `CCUSAGE_GAUGE_CONFIG_HOME`, `CCUSAGE_GAUGE_STATE_HOME`, `CCUSAGE_GAUGE_CACHE_HOME`).
- `Sources/AppCore/CCUsage.swift`, `Sources/AppCore/Snapshot.swift`, `Sources/AppCore/Machines.swift`, `Sources/AppCore/MachineCollectionModels.swift`, `Sources/AppCore/RateLimitUsageModels.swift`, `Sources/AppCore/PieChartSelection.swift`, `Sources/AppCore/DashboardAPIModels.swift`, `Sources/AppCore/UsageEffort.swift`: exact JSON field names (camelCase), decimal serialization, ISO 8601 timestamps, and domain record shapes.

## Non-Goals

- Do NOT delete Swift files (`Package.swift`, `Sources/`, `Tests/`), update `mise.toml`, or touch `frontend/`.
- Do NOT implement full scanner, pricing, SQLite, rate-limit, SSH, aggregation, dashboard router, CLI subcommand, or Tauri tray logic here (only minimal compiling module stubs).
- Do NOT leave `src-tauri/Cargo.lock` behind; remove it once consolidated into the root `Cargo.lock`.

## Target Files, Symbols, and Contracts

1. **`.gitignore`, Root `Cargo.toml`, & `Cargo.lock`**:
   - Add `/target/` to `.gitignore` immediately in Wave 1 so Cargo build outputs never appear as untracked files in subsequent waves.
   - `[workspace] resolver = "2"`, `members = ["crates/ccusage-core", "crates/ccusage-cli", "src-tauri"]`.
   - `[workspace.package] version = "0.5.1"`, `edition = "2021"`, `license = "MIT"`.
   - `[workspace.dependencies]`: pin every crate needed across all subsequent plans so later waves run `--locked` without mutating `Cargo.lock`: `serde` (`derive`), `serde_json`, `rust_decimal` (`serde`, `serde-with-float`, `serde-with-str`), `chrono` (`serde`), `chrono-tz`, `iana-time-zone`, `rusqlite` (`bundled`), `tokio` (`full`), `reqwest` (`default-features = false`, `rustls-tls`, `json`), `hyper` (`server`, `http1`), `hyper-util` (`tokio`, `http1`), `http-body-util`, `portable-pty`, `libc`, `uuid` (`v4`), `thiserror`, `percent-encoding`, `url`, `sha2`, `clap` (`derive`), `png`, `tiny-skia`, `tauri` (`version = "2"`, `features = ["custom-protocol", "tray-icon", "image-rgba"]`, matching cached `2.11.5`), `tauri-build` (`version = "2"`), `tempfile`.
   - macOS-only in `src-tauri/Cargo.toml`: `[target.'cfg(target_os = "macos")'.dependencies] objc2 = "0.6"` (for `SMAppService.mainAppService`).
2. **`crates/ccusage-core/Cargo.toml`**: library crate `ccusage-core` with all non-GUI workspace dependencies (zero Tauri/AppKit/WebKit linkage so Linux amd64 builds stay headless).
3. **`crates/ccusage-cli/Cargo.toml` & `crates/ccusage-cli/src/main.rs`**: binary `ccusage-gauge` depending on `ccusage-core` + `clap` + `tokio` + `serde` + `serde_json`; minimal compiling `main()` stub.
4. **`src-tauri/Cargo.toml`, `src-tauri/src/lib.rs`, `src-tauri/src/tray_icons.rs`**:
   - Package `name = "ccusage-gauge-app"`, `[lib] name = "ccusage_gauge_app" path = "src/lib.rs"` exporting `pub mod tray_icons;`, and `[[bin]] name = "ccusage-gauge-app" path = "src/main.rs"`.
   - Keep existing `src-tauri/src/main.rs` and `src-tauri/src/bridge.rs` compiling untouched in Wave 1.
   - Delete `src-tauri/Cargo.lock` so the root `Cargo.lock` is authoritative.
5. **`crates/ccusage-core/src/version.rs`**: `pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");`.
6. **`crates/ccusage-core/src/paths.rs`**:
   - `pub struct AppPaths { pub config_dir: PathBuf, pub config_file: PathBuf, pub machines_file: PathBuf, pub state_dir: PathBuf, pub state_file: PathBuf, pub rate_limits_file: PathBuf, pub log_dir: PathBuf, pub cache_dir: PathBuf, pub legacy_aggregation_cache_file: PathBuf, pub dashboard_state_file: PathBuf, pub litellm_pricing_cache_file: PathBuf }`.
   - `AppPaths::production()`, `AppPaths::from_env(env: &HashMap<String, String>, home_dir: &Path) -> AppPaths`, `AppPaths::aggregation_cache_file_for_machine(&self, machine_id: &str) -> Result<PathBuf, PathError>` (validating canonical machine slug before path construction).
   - Exact root-then-app-dir override semantics matching `Sources/AppCore/Configuration.swift:287-301` and all smoke/E2E/emulation scripts:
     - `base_home = CCUSAGE_GAUGE_HOME ?? home_dir`
     - `config_root = CCUSAGE_GAUGE_CONFIG_HOME ?? base_home.join(".config")`
     - `state_root = CCUSAGE_GAUGE_STATE_HOME ?? base_home.join(".local")`
     - `cache_root = CCUSAGE_GAUGE_CACHE_HOME ?? base_home.join(".cache")`
     - **ALWAYS** append `ccusage-gauge/` beneath each resolved root:
       - `config_dir = config_root.join("ccusage-gauge")` (`config_file = config_dir/ccusage-config.json`, `machines_file = config_dir/machines.json`)
       - `state_dir = state_root.join("ccusage-gauge")` (`state_file = state_dir/state.json`, `rate_limits_file = state_dir/rate-limits.json`, `log_dir = state_dir/logs`)
       - `cache_dir = cache_root.join("ccusage-gauge")` (`legacy_aggregation_cache_file = cache_dir/aggregates.sqlite3`, `dashboard_state_file = cache_dir/dashboard-state.sqlite3`, `litellm_pricing_cache_file = cache_dir/litellm-pricing.json`)
     - Never cache `AppPaths` in process-global statics.
7. **`crates/ccusage-core/src/types.rs`**: Pin EVERY shared domain model, JSON serde contract (exact camelCase and uppercase-acronym names matching `Sources/AppCore/*.swift` and `frontend/src/api.ts`, decimal accepting JSON number or numeric string, ISO 8601 UTC timestamps, `#[serde(skip_serializing_if = "Option::is_none")]` on optional fields where Swift omits `nil` and preserving explicit `null` where Swift encodes `nil` such as `RateLimitWindow`/`RateLimitCredits`/`ProviderRateLimits`), and cross-module trait signature up front so Wave 2 workers never need to modify `types.rs`:
   ```rust
   // Usage & Scanner contracts (Sources/AppCore/CCUsage.swift:117-120, UsageEffort.swift, frontend/src/api.ts:41,60)
   #[serde(rename_all = "camelCase")]
   pub enum UsageDataQuality { Timestamped, SessionEstimated } // serde & SQLite: "timestamped", "sessionEstimated" (daily CostRow uses "daily" at the dashboard query DTO layer)
   pub struct UsageEffort; // UsageEffort::normalize(&str) -> Option<String> (trimmed lowercase, 1..=32 ASCII [a-z0-9_-])
   pub struct TimestampedUsageEvent {
       pub identity: String, pub timestamp: DateTime<Utc>, pub agent: String,
       pub session_id: String, pub request_id: String, pub message_id: String,
       pub model: String, pub input_tokens: u64, pub output_tokens: u64,
       pub cache_creation_tokens: u64, pub cache_read_tokens: u64,
       pub cache_creation_5m_tokens: u64, pub cache_creation_1h_tokens: u64,
       pub directory: Option<String>, pub effort: Option<String>,
   }
   pub struct PricedUsageEvent { pub event: TimestampedUsageEvent, pub cost_usd: Decimal }
   pub struct ScanStats { pub files_reused: u64, pub files_resumed: u64, pub files_rescanned: u64, pub bytes_read: u64 }
   pub enum ScanFileTransition {
       Reuse,
       Resume { prev_offset: u64, new_offset: u64, new_events: Vec<TimestampedUsageEvent> },
       Rescan,
   }
   // Snapshot & Aggregate contracts (Sources/AppCore/CCUsage.swift:3-190, ResetWindow.swift:76-100, State.swift:3-83, Snapshot.swift:330-386, Configuration.swift:83-120)
   // Acronym JSON keys: costUSD, costSinceResetUSD, budgetUSD, spentUSD, remainingUSD, overageUSD, budgetMachineIDs, totalTokens
   pub struct CostRecord { pub timestamp: DateTime<Utc>, /* "costUSD" */ pub cost_usd: Decimal, pub models: Vec<String>, pub machine: String, /* skip_none */ pub directory: Option<String> }
   pub struct MetricRecord { pub date: String, pub agent: String, pub model: String, /* "costUSD" */ pub cost_usd: Decimal, pub input_tokens: u64, pub output_tokens: u64, pub cache_creation_tokens: u64, pub cache_read_tokens: u64, /* serialized as "totalTokens" = sum of 4 token fields; recomputed on decode */ pub total_tokens: u64, pub machine: String, /* skip_none */ pub directory: Option<String>, /* skip_none */ pub effort: Option<String> }
   pub struct SessionMetricRecord { pub timestamp: DateTime<Utc>, pub agent: String, pub model: String, /* "costUSD" */ pub cost_usd: Decimal, pub input_tokens: u64, pub output_tokens: u64, pub cache_creation_tokens: u64, pub cache_read_tokens: u64, /* serialized as "totalTokens" = sum of 4 token fields; recomputed on decode */ pub total_tokens: u64, pub data_quality: UsageDataQuality, pub machine: String, /* skip_none */ pub directory: Option<String>, /* skip_none */ pub effort: Option<String> }
   // Custom internally-tagged serde matching Configuration.swift:83-120:
   // {"type":"hourly"|"daily"|"weekly"|"monthly"} and {"type":"customHours","hours":N} (reject hours == 0 on decode)
   pub enum ResetCycle { Hourly, Daily, Weekly, Monthly, CustomHours(u32) }
   pub struct ResetBaseline { pub scheduled_boundary_at: DateTime<Utc>, pub active_boundary_at: DateTime<Utc>, pub cycle: ResetCycle, pub calendar_identifier: String, pub time_zone_identifier: String, pub computed_at: DateTime<Utc> }
   pub struct BudgetSummary { /* "budgetUSD" */ pub budget_usd: Option<Decimal>, /* "spentUSD" */ pub spent_usd: Decimal, /* "remainingUSD" */ pub remaining_usd: Option<Decimal>, /* "overageUSD" */ pub overage_usd: Decimal, pub usage_percentage: Option<Decimal>, pub visual_fraction: Option<Decimal> }
   pub struct CostSnapshot { pub generated_at: DateTime<Utc>, pub active_boundary_at: DateTime<Utc>, /* "costSinceResetUSD" */ pub cost_since_reset_usd: Decimal, pub budget: BudgetSummary, pub reset_cycle: ResetCycle, pub refresh_interval_seconds: u32, pub points: Vec<CostRecord>, pub dashboard_metrics: Vec<MetricRecord>, pub dashboard_sessions: Vec<SessionMetricRecord>, #[serde(skip)] pub rolling_events: Vec<PricedUsageEvent> }
   // Config, State, PieChart, Cache, Dashboard UI contracts (Configuration.swift, PieChartSelection.swift, State.swift, AggregationCache.swift, DashboardStateStore.swift, frontend/src/api.ts:95,111-126,192-193)
   pub struct ChartColorScheme { pub machines: BTreeMap<String, String>, pub models: BTreeMap<String, String> }
   pub struct ChartColors { pub light: ChartColorScheme, pub dark: ChartColorScheme } // decode accepts legacy flat {machines, models} copying to both light & dark
   pub struct AppConfiguration { pub ccusage_path: Option<String>, pub default_reset_term: String, pub dashboard_port: u16, pub dashboard_autostart: bool, pub poll_interval_seconds: u32, pub cache_retention_days: u32, pub remote_retry_count: u32, pub remote_retry_delay_seconds: u32, pub remote_timeout_seconds: u32, pub chart_colors: ChartColors }
   #[serde(rename_all = "camelCase")]
   pub enum PieChartId { Budget, Claude5h, ClaudeWeekly, ClaudeCredit, Codex5h, CodexWeekly, CodexCredit, Gemini5h, GeminiWeekly }
   // PieChartId::ALL (9 variants), PieChartId::menu_bar_cases() (7 non-credit variants), is_credit(), provider() -> Option<RateLimitProvider>
   pub struct MenuBarStatusEntry { pub id: PieChartId, pub fraction: Option<f64>, pub show_icon: bool, pub ratio_label: Option<String>, pub is_budget_unset: bool, pub is_budget_unknown: bool }
   pub struct AppState { /* "budgetUSD" */ pub budget_usd: Option<Decimal>, /* "budgetMachineIDs" */ pub budget_machine_ids: Vec<String>, pub reset_cycle: ResetCycle, pub baseline: Option<ResetBaseline>, pub refresh_interval_seconds: Option<u32>, pub menu_bar_pie_charts: Vec<PieChartId>, pub menu_bar_ratio_labels: Vec<PieChartId>, pub menu_bar_show_cost: bool, pub menu_bar_cost_only: bool }
   pub struct AggregationCacheRange { pub since: String, pub through: String }
   pub struct AggregationCacheJob { pub since: String, pub through: String }
   pub struct AggregationCachePayload { pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>, pub cached_from: String, pub cached_through: String, pub metrics: Vec<MetricRecord>, pub sessions: Vec<SessionMetricRecord>, pub covered_ranges: Vec<AggregationCacheRange>, pub directory_covered_ranges: Vec<AggregationCacheRange>, pub antigravity_covered_ranges: Vec<AggregationCacheRange>, pub source_configuration_fingerprint: Option<String> }
   pub enum LegacyPartitionDecision { Complete, PartialOrUnverifiable, Absent }
   pub struct DashboardUIState { pub range: String, pub custom_start: String, pub custom_end: String, pub selected_models: Vec<String>, pub selected_agents: Vec<String>, pub selected_machines: Vec<String>, pub granularity: String, pub chart_metric: String, pub stack_by: String, pub sidebar_collapsed: bool, pub header_collapsed: bool, pub pie_charts: Vec<PieChartId>, pub hidden_pie_ratios: Vec<PieChartId> }
   // Machine & SSH contracts (Machines.swift:8-214, MachineCollectionModels.swift:3-180, MachineStatusCoding.swift:3-95, frontend/src/api.ts:18-51,127-155)
   #[serde(rename_all = "camelCase")]
   pub enum MachineKind { Local, Ssh }
   pub struct LocalSessionSources { pub codex_session_dirs: Vec<String>, pub claude_config_dirs: Vec<String>, pub include_default_codex_dir: bool, pub include_default_claude_dir: bool }
   #[serde(rename_all = "camelCase")]
   pub struct SSHJumpProxy { pub host: String, pub port: u16, pub user: String, /* skip_none */ pub identity_file: Option<String>, /* skip_none */ pub known_hosts_file: Option<String> }
   #[serde(tag = "kind", rename_all = "camelCase")]
   pub enum SSHProxy { Direct, Jump(SSHJumpProxy), Command { executable: String } }
   // Wire shapes: {"kind":"direct"}, {"kind":"jump","host":...,"port":22,"user":...,"identityFile":...,"knownHostsFile":...}, {"kind":"command","executable":...}
   pub struct SSHConnection { pub host: String, pub port: u16, pub user: String, /* skip_none */ pub identity_file: Option<String>, pub extra_options: Vec<String>, pub remote_ccusage_path: String, /* skip_none */ pub proxy: Option<SSHProxy> }
   pub struct MachineDescriptor { pub id: String, pub display_name: String, pub kind: MachineKind, pub enabled: bool, /* skip_none */ pub ssh: Option<SSHConnection>, pub codex_session_dirs: Vec<String>, pub claude_config_dirs: Vec<String>, pub include_default_codex_dir: bool, pub include_default_claude_dir: bool }
   pub struct MachineRegistry { pub schema_version: u32, pub local_session_sources: LocalSessionSources, pub machines: Vec<MachineDescriptor> }
   pub struct MachineSessionSourcePlan { pub machine_id: String, pub codex_roots: Vec<PathBuf>, pub claude_roots: Vec<PathBuf>, pub fingerprint: String }
   #[serde(rename_all = "camelCase")]
   pub enum MachineCollectionState { Disabled, NeverCollected, Healthy, Stale, Error }
   pub type MachineHealthState = MachineCollectionState;
   #[serde(rename_all = "camelCase")]
   pub struct SanitizedCollectionError { pub code: String, pub message: String, /* skip_none */ pub detail: Option<String>, /* skip_none */ pub remediation: Option<String> }
   pub type MachineDiagnostic = SanitizedCollectionError;
   #[serde(rename_all = "camelCase")]
   pub struct MachineStatusDataGap { pub start_at: DateTime<Utc>, pub end_at: DateTime<Utc> }
   #[serde(rename_all = "camelCase")]
   pub struct MachineStatus { pub id: String, pub display_name: String, pub kind: MachineKind, pub enabled: bool, pub collection_state: MachineCollectionState, pub snapshot_available: bool, pub collection_in_progress: bool, pub stale: bool, /* skip_none */ pub coverage_start: Option<String>, /* skip_none */ pub snapshot_generated_at: Option<DateTime<Utc>>, /* skip_none */ pub last_attempt_at: Option<DateTime<Utc>>, /* skip_none */ pub last_success_at: Option<DateTime<Utc>>, pub consecutive_failure_count: u32, /* skip_none */ pub unavailable_since: Option<DateTime<Utc>>, /* skip_none */ pub stale_since: Option<DateTime<Utc>>, /* skip_none */ pub last_error_at: Option<DateTime<Utc>>, /* skip_none */ pub last_error: Option<SanitizedCollectionError>, /* skip_none */ pub last_hour_data_gap: Option<MachineStatusDataGap>, pub refresh_interval_seconds: u32 }
   #[serde(rename_all = "camelCase")]
   pub struct MachineLatestEvent { pub machine: String, /* skip_none */ pub latest_event_at: Option<DateTime<Utc>>, pub marker_state: String, pub in_last_hour: bool, /* skip_none */ pub data_quality: Option<UsageDataQuality> }
   #[serde(rename_all = "camelCase")]
   pub enum DashboardDataDisposition { Current, Historical }
   #[serde(rename_all = "snake_case")]
   pub enum MachineAvailabilityReason { CollectionStale, NeverCollected, TunnelUnreachable, AuthFailed, HostKeyVerificationFailed, Timeout, TransportFailed, RemoteCommandFailed, InvalidResponse, CacheFailed, ExecutableUnavailable, InsufficientCoverage, InternalError }
   #[serde(rename_all = "camelCase")]
   pub struct MachineAvailability { pub machine: String, pub available: bool, pub unavailable_since: DateTime<Utc>, pub reason_code: MachineAvailabilityReason }
   #[serde(rename_all = "camelCase")]
   pub struct MachineDataGap { pub machine: String, pub start_at: DateTime<Utc>, pub end_at: DateTime<Utc>, pub reason_code: MachineAvailabilityReason }
   #[serde(rename_all = "camelCase")]
   pub struct DashboardScope { pub requested: String, pub data_disposition: DashboardDataDisposition, pub included_machine_ids: Vec<String>, pub stale_machine_ids: Vec<String>, pub unavailable_machine_ids: Vec<String>, pub excluded_from_current_totals_machine_ids: Vec<String>, pub machine_availability: Vec<MachineAvailability>, pub last_hour_data_gaps: Vec<MachineDataGap>, pub evaluated_at: DateTime<Utc>, /* skip_none */ pub generated_at: Option<DateTime<Utc>> }
   #[serde(rename_all = "camelCase")]
   pub struct DashboardRangeLoadProgress { /* skip_none */ pub requested_start: Option<DateTime<Utc>>, /* skip_none */ pub requested_end: Option<DateTime<Utc>>, pub completed: usize, pub total: usize, pub is_loading: bool, pub is_partial: bool, pub failed_machine_ids: Vec<String> }
   // Rate-limit contracts (Sources/AppCore/RateLimitUsageModels.swift:3-155, frontend/src/api.ts:96-110)
   #[serde(rename_all = "camelCase")]
   pub enum RateLimitProvider { Claude, Codex, Gemini }
   #[serde(rename_all = "camelCase")]
   pub enum RateLimitStatus { Ok, Stale, Unauthorized, Unavailable }
   #[serde(rename_all = "snake_case")]
   pub enum RateLimitErrorCode { CliMissing, CredentialsMissing, CredentialsUnreadable, TokenExpired, Unauthorized, RateLimited, HttpError, NetworkError, Timeout, InvalidResponse }
   #[serde(rename_all = "camelCase")]
   pub enum RateLimitWindowKind { FiveHour, Weekly, Other } // classify(window_seconds): 18_000 => FiveHour, 604_800 => Weekly, _ => Other
   #[serde(rename_all = "camelCase")]
   pub struct RateLimitWindow { pub kind: RateLimitWindowKind, pub window_seconds: u64, pub used_percent: f64, pub remaining_percent: f64, pub resets_at: Option<DateTime<Utc>> }
   #[serde(rename_all = "camelCase")]
   pub enum RateLimitCreditUnit { Usd, Credits }
   #[serde(rename_all = "camelCase")]
   pub struct RateLimitCredits { pub unit: RateLimitCreditUnit, pub enabled: bool, pub unlimited: bool, pub used: Option<f64>, pub limit: Option<f64>, pub balance: Option<f64>, pub used_percent: Option<f64> }
   #[serde(rename_all = "camelCase")]
   pub struct RateLimitObservation { pub plan_type: Option<String>, pub observed_at: DateTime<Utc>, pub windows: Vec<RateLimitWindow>, pub credits: Option<RateLimitCredits> }
   pub struct RateLimitWindowSlots { pub five_hour: Option<RateLimitWindow>, pub weekly: Option<RateLimitWindow>, pub other: Vec<RateLimitWindow> }
   #[serde(rename_all = "camelCase")]
   pub struct ProviderRateLimits { pub provider: RateLimitProvider, pub status: RateLimitStatus, pub error_code: Option<RateLimitErrorCode>, pub message: Option<String>, pub plan_type: Option<String>, pub fetched_at: Option<DateTime<Utc>>, pub stale: bool, pub five_hour: Option<RateLimitWindow>, pub weekly: Option<RateLimitWindow>, pub other_windows: Vec<RateLimitWindow>, pub credits: Option<RateLimitCredits> }
   #[serde(rename_all = "camelCase")]
   pub struct RateLimitsResponse { pub generated_at: DateTime<Utc>, pub scope: String, pub providers: Vec<ProviderRateLimits> } // scope == "local"
   // Cross-module traits (preventing Wave 2 mutual imports)
   pub trait DirectoryNameLifecycle: Send + Sync {
       fn prepare_delete(&self, machine_id: &str) -> Result<(), String>;
       fn commit_delete(&self, machine_id: &str) -> Result<(), String>;
       fn rollback_delete(&self, machine_id: &str) -> Result<(), String>;
       fn purge_deleted(&self, machine_id: &str) -> Result<(), String>;
       fn purge_for_reuse(&self, machine_id: &str) -> Result<(), String>;
       fn reconcile_machine_registry(&self, machine_ids: &[String]) -> Result<(), String>;
   }
   pub trait MachineRuntimeReconciler: Send + Sync {
       fn reconcile_registry<'a>(&'a self, registry: &'a MachineRegistry, revision: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
   }
   pub trait PricingHttpTransport: Send + Sync {
       fn get_text<'a>(&'a self, url: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;
   }
   ```
   (Verify every JSON field name against the exact Swift reference files and `frontend/src/api.ts` cited above so JSON wire and SQLite parity is byte-accurate.)
8. **`crates/ccusage-core/src/lib.rs` & module stubs**: declare `pub mod version; pub mod paths; pub mod types; pub mod config; pub mod state; pub mod scanner; pub mod pricing; pub mod cache; pub mod aggregation; pub mod rate_limits; pub mod remote; pub mod dashboard;` with compiling `mod.rs` stubs for each Wave 2+ module.

## Key Invariants and Pitfalls

- **Decimal serde compatibility**: Swift `state.json`, API responses, and fixtures encode `Decimal` as JSON numbers (and may supply numeric strings). Use a custom serde helper accepting either JSON number or numeric string on decode, and emitting exact numeric JSON without floating-point drift.
- **No static path caching**: Tests isolate `CCUSAGE_GAUGE_*` per test case; `AppPaths` must resolve fresh from environment/inputs every call.
- **Slug validation in `aggregation_cache_file_for_machine`**: Reject empty strings, `/`, `\`, `..`, uppercase, or non-`[a-z0-9-]` characters so callers cannot cause path traversal.
- **Keep `src-tauri` building**: Ensure `frontend/dist` exists (run `cd frontend && bun run build` only if absent, without changing tracked frontend files) before verifying `cargo check --workspace`.
- **Keep Rust files under 1000 lines**.

## Test Cases (`crates/ccusage-core/tests/paths_types_tests.rs`)

- Default `AppPaths::from_env` with only `HOME` -> resolves `~/.config/ccusage-gauge`, `~/.local/ccusage-gauge`, `~/.cache/ccusage-gauge`, `logs`, `aggregates-local.sqlite3`, `dashboard-state.sqlite3`, `litellm-pricing.json`.
- `CCUSAGE_GAUGE_HOME` override -> relocates config, state, logs, and cache roots together, still appending `ccusage-gauge/` under `.config`, `.local`, and `.cache`.
- Explicit `CCUSAGE_GAUGE_CONFIG_HOME=/tmp/cfg` / `CCUSAGE_GAUGE_STATE_HOME=/tmp/state` / `CCUSAGE_GAUGE_CACHE_HOME=/tmp/cache` -> override `CCUSAGE_GAUGE_HOME` independently AND always append `ccusage-gauge/`: asserts `config_file == "/tmp/cfg/ccusage-gauge/ccusage-config.json"`, `state_file == "/tmp/state/ccusage-gauge/state.json"`, `log_dir == "/tmp/state/ccusage-gauge/logs"`, and `dashboard_state_file == "/tmp/cache/ccusage-gauge/dashboard-state.sqlite3"`.
- `aggregation_cache_file_for_machine("../escape")` or `"bad/id"` -> returns validation error; `"local"` and `"prod-box-1"` -> valid paths.
- Wire & storage JSON round-trips matching Swift & `frontend/src/api.ts`:
  - `UsageDataQuality` -> `"timestamped"` and `"sessionEstimated"` (never `"dailyFallback"`).
  - `ResetCycle` -> internally tagged `{"type":"hourly"}`..`{"type":"monthly"}` and `{"type":"customHours","hours":240}`; rejects `{"type":"customHours","hours":0}`.
  - `MetricRecord` & `SessionMetricRecord` -> serialize exact `costUSD` and `totalTokens` (`input + output + cacheCreation + cacheRead`), recompute `totalTokens` on decode, accept both numeric and string decimals, omit `None` optional `directory`/`effort`, and `CostSnapshot` omits internal `rolling_events`.
  - `PieChartId` -> all 9 variants (`budget`, `claude5h`, `claudeWeekly`, `claudeCredit`, `codex5h`, `codexWeekly`, `codexCredit`, `gemini5h`, `geminiWeekly`) plus `menu_bar_cases()` excluding only `ClaudeCredit`/`CodexCredit`.
  - `SSHProxy` -> `{"kind":"direct"}`, `{"kind":"jump","host":"bastion","port":22,"user":"ops","knownHostsFile":"/etc/ssh/known_hosts"}`, and `{"kind":"command","executable":"/usr/bin/nc"}`.
  - `SanitizedCollectionError` -> includes optional `detail` and `remediation`; `MachineLatestEvent` -> serializes `{machine, latestEventAt, markerState, inLastHour, dataQuality}`.
  - `RateLimitObservation`, `ProviderRateLimits`, `RateLimitsResponse` -> round-trip `planType`, `observedAt`, `windows` (`kind`, `windowSeconds`, `usedPercent`, `remainingPercent`, `resetsAt`), `credits` (`unit`, `enabled`, `unlimited`, `used`, `limit`, `balance`, `usedPercent`), `fiveHour`, `weekly`, `otherWindows`, `errorCode` (`snake_case`), `scope: "local"`.
- `UsageEffort::normalize` -> accepts `xhigh`, `high`, `medium`, `low`, `max`, `minimal` case-insensitively (any 1..=32 ASCII `[a-z0-9_-]`) and returns `None` for blank/illegal values.

## Verification Commands and Acceptance Criteria

```bash
cargo check --workspace
cargo test --locked -p ccusage-core --test paths_types_tests
```

Done when:
1. Root `Cargo.toml` and `Cargo.lock` exist, `src-tauri/Cargo.lock` is removed, and `cargo check --workspace` exits `0`.
2. `cargo test --locked -p ccusage-core --test paths_types_tests` exits `0` with all test cases passing.
3. No Swift files or `frontend/` tracked files were modified.

## Amendment 2026-11-26 (blocked-wave-scope-or-contract, round 1)

- **Blocker resolved**: `Approved Cargo build and download artifact location` (`artifactRoots: []` with Cargo verification build outputs and uncached `rusqlite`/`portable-pty` crate downloads).
- **Authorized artifact roots**:
  - `target` (added to `writePaths` and `artifactRoots`, gitignored via `/target/`): authorized for all Cargo build and test outputs (`cargo check --workspace`, `cargo test --locked -p ccusage-core --test paths_types_tests`).
  - `tmp/cargo-home` (added to `writePaths` and `artifactRoots`, gitignored via `tmp/`): authorized repo-local fallback Cargo registry/download cache (`CARGO_HOME=tmp/cargo-home`) if the ambient external Cargo cache is unavailable or read-only.
- **External Cargo cache & temp scratch authority**:
  - Prefer the ambient external Cargo cache (`${CARGO_HOME:-$HOME/.cargo}`, e.g. `/Users/taco/.cargo`) when writable to reuse already-cached crates and download only missing dependencies (`rusqlite`, `portable-pty`, and any other uncached workspace crates required to generate root `Cargo.lock`). If `${CARGO_HOME:-$HOME/.cargo}` is read-only or restricted, run Cargo commands with `CARGO_HOME=tmp/cargo-home` and leave `tmp/cargo-home` populated (gitignored) for read/lock reuse in Waves 2..7.
  - External OS temp scratch (`$TMPDIR` / `tempfile`) is authorized for test fixtures.
  - Never stage or commit `target/` or `tmp/cargo-home/`.

## Progress Log

Record progress in a dedicated worker progress note (do not modify `impl-plans/active/` or run concurrent git commands). Read files fresh before editing and verify no unintended changes outside `writePaths`.

- Step 6 partial: workspace manifests and module seams created; shared paths/types, root lockfile, tests, and verification remain. No artifact root is declared for Cargo build outputs or uncached dependency downloads; checkpoint amendment required before Cargo commands. Existing nested lock retained until root lock is generated. Formal review and finalization remain downstream.

- Step 6 continuation: root lockfile generated and nested lock removed; workspace check succeeds after removing the nonexistent Tauri `image-rgba` feature. Implemented path resolution, initial usage/reset/rate-limit types, and two focused tests. The shared domain contracts and the full parity test matrix remain incomplete; this plan is not ready for review or dependent waves. Formal review and finalization remain downstream.

- Step 6 continuation 1: implemented the remaining shared usage, aggregate, configuration/state, cache, machine/SSH, dashboard and rate-limit contracts and cross-module traits. Added exact decimal, token-total recomputation, legacy state/color, path and wire parity tests. Workspace build, focused tests, rustfmt, and workspace clippy are rerun on the final source before handoff. Implementation acceptance is complete when those final gates pass; independent review and finalization remain downstream.

- Step 6 final source gates: `cargo check --workspace` passed; `cargo test --locked -p ccusage-core --test paths_types_tests` passed 11 tests; `cargo clippy --locked --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` passed. No tracked Swift or frontend files changed. Implementation criteria are met; independent review and finalization remain pending.

- Adversarial review revision: fixed DashboardUIState legacy defaults and chart normalization with regression coverage; added pinned direct dependencies to the Tauri and headless CLI members and refreshed the root lockfile offline. Locked workspace check, 12 parity tests, clippy, formatting, and headless dependency-tree audit passed. Independent re-review remains pending.

- Redispatch revision: added scientific-notation fallback and required/optional decimal wire regression coverage, including exact serialization and invalid inputs. The new test also passed before the fallback with the locked dependencies. Locked workspace check, 13 parity tests, workspace clippy, rustfmt, and headless CLI audit passed; Swift and frontend tracked files remain unchanged. Implementation criteria are met; independent test-integrity and adversarial reviews remain pending.
