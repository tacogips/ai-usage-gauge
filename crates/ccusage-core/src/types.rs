use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageDataQuality {
    Timestamped,
    SessionEstimated,
}

pub struct UsageEffort;
impl UsageEffort {
    pub fn normalize(value: &str) -> Option<String> {
        let value = value.trim().to_ascii_lowercase();
        if (1..=32).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            Some(value)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "hours", rename_all = "camelCase")]
pub enum ResetCycle {
    Hourly,
    Daily,
    Weekly,
    Monthly,
    CustomHours(u32),
}
impl<'de> Deserialize<'de> for ResetCycle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", content = "hours", rename_all = "camelCase")]
        enum Repr {
            Hourly,
            Daily,
            Weekly,
            Monthly,
            CustomHours(u32),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Hourly => Self::Hourly,
            Repr::Daily => Self::Daily,
            Repr::Weekly => Self::Weekly,
            Repr::Monthly => Self::Monthly,
            Repr::CustomHours(0) => return Err(serde::de::Error::custom("hours must be positive")),
            Repr::CustomHours(hours) => Self::CustomHours(hours),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateLimitProvider {
    Claude,
    Codex,
    Gemini,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateLimitStatus {
    Ok,
    Stale,
    Unauthorized,
    Unavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitErrorCode {
    CliMissing,
    CredentialsMissing,
    CredentialsUnreadable,
    TokenExpired,
    Unauthorized,
    RateLimited,
    HttpError,
    NetworkError,
    Timeout,
    InvalidResponse,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateLimitWindowKind {
    FiveHour,
    Weekly,
    Other,
}
impl RateLimitWindowKind {
    pub fn classify(seconds: u64) -> Self {
        match seconds {
            18_000 => Self::FiveHour,
            604_800 => Self::Weekly,
            _ => Self::Other,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub kind: RateLimitWindowKind,
    pub window_seconds: u64,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateLimitCreditUnit {
    Usd,
    Credits,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitCredits {
    pub unit: RateLimitCreditUnit,
    pub enabled: bool,
    pub unlimited: bool,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub balance: Option<f64>,
    pub used_percent: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitObservation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub windows: Vec<RateLimitWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<RateLimitCredits>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRateLimits {
    pub provider: RateLimitProvider,
    pub status: RateLimitStatus,
    pub error_code: Option<RateLimitErrorCode>,
    pub message: Option<String>,
    pub plan_type: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub five_hour: Option<RateLimitWindow>,
    pub weekly: Option<RateLimitWindow>,
    pub other_windows: Vec<RateLimitWindow>,
    pub credits: Option<RateLimitCredits>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsResponse {
    pub generated_at: DateTime<Utc>,
    pub scope: String,
    pub providers: Vec<ProviderRateLimits>,
}

use rust_decimal::Decimal;
use std::{collections::BTreeMap, path::PathBuf};

mod decimal_number {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::str::FromStr;
    pub fn serialize<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        let number =
            serde_json::Number::from_str(&value.to_string()).map_err(serde::ser::Error::custom)?;
        number.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let text = match value {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s,
            _ => {
                return Err(serde::de::Error::custom(
                    "expected decimal number or string",
                ))
            }
        };
        Decimal::from_str(&text)
            .or_else(|_| Decimal::from_scientific(&text))
            .map_err(serde::de::Error::custom)
    }
}
mod optional_decimal_number {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => super::decimal_number::serialize(value, serializer),
            None => serializer.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Decimal>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        value
            .map(|v| super::decimal_number::deserialize(v).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimestampedUsageEvent {
    pub identity: String,
    pub timestamp: DateTime<Utc>,
    pub agent: String,
    pub session_id: String,
    pub request_id: String,
    pub message_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricedUsageEvent {
    pub event: TimestampedUsageEvent,
    #[serde(rename = "costUSD", with = "decimal_number")]
    pub cost_usd: Decimal,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files_reused: u64,
    pub files_resumed: u64,
    pub files_rescanned: u64,
    pub bytes_read: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub enum ScanFileTransition {
    Reuse,
    Resume {
        prev_offset: u64,
        new_offset: u64,
        new_events: Vec<TimestampedUsageEvent>,
    },
    Rescan,
}
fn local_machine() -> String {
    "local".to_owned()
}
fn decode_machine<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let machine = Option::<String>::deserialize(deserializer)?.unwrap_or_else(local_machine);
    if machine.is_empty() {
        return Err(serde::de::Error::custom("machine must not be empty"));
    }
    Ok(machine)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostRecord {
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "costUSD", with = "decimal_number")]
    pub cost_usd: Decimal,
    pub models: Vec<String>,
    #[serde(default = "local_machine", deserialize_with = "decode_machine")]
    pub machine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

macro_rules! metric_record {
    ($name:ident, $time:ident: $time_type:ty $(, $extra:ident: $extra_type:ty)*) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            pub $time: $time_type, pub agent: String, pub model: String, pub cost_usd: Decimal,
            pub input_tokens: u64, pub output_tokens: u64, pub cache_creation_tokens: u64,
            pub cache_read_tokens: u64, pub total_tokens: u64, $(pub $extra: $extra_type,)*
            pub machine: String, pub directory: Option<String>, pub effort: Option<String>,
        }
        const _: () = {
            #[derive(Serialize, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Wire {
                $time: $time_type, agent: String, model: String,
                #[serde(rename = "costUSD", with = "decimal_number")] cost_usd: Decimal,
                input_tokens: u64, output_tokens: u64, cache_creation_tokens: u64, cache_read_tokens: u64,
                #[serde(default, skip_deserializing)] total_tokens: u64, $($extra: $extra_type,)* #[serde(default = "local_machine", deserialize_with = "decode_machine")] machine: String,
                #[serde(skip_serializing_if = "Option::is_none")] directory: Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")] effort: Option<String>,
            }
            impl Serialize for $name {
                fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    let total_tokens = self.input_tokens.checked_add(self.output_tokens).and_then(|v| v.checked_add(self.cache_creation_tokens)).and_then(|v| v.checked_add(self.cache_read_tokens)).ok_or_else(|| serde::ser::Error::custom("token total overflow"))?;
                    Wire { $time: self.$time.clone(), agent: self.agent.clone(), model: self.model.clone(), cost_usd: self.cost_usd,
                        input_tokens: self.input_tokens, output_tokens: self.output_tokens, cache_creation_tokens: self.cache_creation_tokens,
                        cache_read_tokens: self.cache_read_tokens, total_tokens,
                        $($extra: self.$extra.clone(),)* machine: self.machine.clone(), directory: self.directory.clone(), effort: self.effort.clone() }.serialize(serializer)
                }
            }
            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                    let w = Wire::deserialize(deserializer)?;
                    let total_tokens = w.input_tokens.checked_add(w.output_tokens).and_then(|v| v.checked_add(w.cache_creation_tokens)).and_then(|v| v.checked_add(w.cache_read_tokens)).ok_or_else(|| serde::de::Error::custom("token total overflow"))?;
                    Ok(Self { $time: w.$time, agent: w.agent, model: w.model, cost_usd: w.cost_usd, input_tokens: w.input_tokens, output_tokens: w.output_tokens, cache_creation_tokens: w.cache_creation_tokens, cache_read_tokens: w.cache_read_tokens, total_tokens, $($extra: w.$extra,)* machine: w.machine, directory: w.directory, effort: w.effort.as_deref().and_then(UsageEffort::normalize) })
                }
            }
        };
    };
}
metric_record!(MetricRecord, date: String);
metric_record!(SessionMetricRecord, timestamp: DateTime<Utc>, data_quality: UsageDataQuality);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetBaseline {
    pub scheduled_boundary_at: DateTime<Utc>,
    pub active_boundary_at: DateTime<Utc>,
    pub cycle: ResetCycle,
    pub calendar_identifier: String,
    pub time_zone_identifier: String,
    pub computed_at: DateTime<Utc>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSummary {
    #[serde(
        rename = "budgetUSD",
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal_number"
    )]
    pub budget_usd: Option<Decimal>,
    #[serde(rename = "spentUSD", with = "decimal_number")]
    pub spent_usd: Decimal,
    #[serde(
        rename = "remainingUSD",
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal_number"
    )]
    pub remaining_usd: Option<Decimal>,
    #[serde(rename = "overageUSD", with = "decimal_number")]
    pub overage_usd: Decimal,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal_number"
    )]
    pub usage_percentage: Option<Decimal>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal_number"
    )]
    pub visual_fraction: Option<Decimal>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSnapshot {
    pub generated_at: DateTime<Utc>,
    pub active_boundary_at: DateTime<Utc>,
    #[serde(rename = "costSinceResetUSD", with = "decimal_number")]
    pub cost_since_reset_usd: Decimal,
    pub budget: BudgetSummary,
    pub reset_cycle: ResetCycle,
    pub refresh_interval_seconds: u32,
    pub points: Vec<CostRecord>,
    pub dashboard_metrics: Vec<MetricRecord>,
    pub dashboard_sessions: Vec<SessionMetricRecord>,
    #[serde(skip)]
    pub rolling_events: Vec<PricedUsageEvent>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartColorScheme {
    #[serde(default)]
    pub machines: BTreeMap<String, String>,
    #[serde(default)]
    pub models: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChartColors {
    pub light: ChartColorScheme,
    pub dark: ChartColorScheme,
}
impl<'de> Deserialize<'de> for ChartColors {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected chart colors object"))?;
        if object.contains_key("light") || object.contains_key("dark") {
            let parse = |key: &str| -> Result<ChartColorScheme, D::Error> {
                object
                    .get(key)
                    .filter(|value| !value.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(serde::de::Error::custom)
                    .map(|v| v.unwrap_or_default())
            };
            Ok(Self {
                light: parse("light")?,
                dark: parse("dark")?,
            })
        } else {
            let colors: ChartColorScheme =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self {
                light: colors.clone(),
                dark: colors,
            })
        }
    }
}
fn default_retention() -> u32 {
    365
}
fn default_retries() -> u32 {
    3
}
fn default_retry_delay() -> u32 {
    2
}
fn default_timeout() -> u32 {
    15
}
fn default_true() -> bool {
    true
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ccusage_path: Option<String>,
    pub default_reset_term: String,
    pub dashboard_port: u16,
    pub dashboard_autostart: bool,
    pub poll_interval_seconds: u32,
    #[serde(default = "default_retention")]
    pub cache_retention_days: u32,
    #[serde(default = "default_retries")]
    pub remote_retry_count: u32,
    #[serde(default = "default_retry_delay")]
    pub remote_retry_delay_seconds: u32,
    #[serde(default = "default_timeout")]
    pub remote_timeout_seconds: u32,
    #[serde(default)]
    pub chart_colors: ChartColors,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PieChartId {
    Budget,
    Claude5h,
    ClaudeWeekly,
    ClaudeCredit,
    Codex5h,
    CodexWeekly,
    CodexCredit,
    Gemini5h,
    GeminiWeekly,
}
impl PieChartId {
    pub const ALL: [Self; 9] = [
        Self::Budget,
        Self::Claude5h,
        Self::ClaudeWeekly,
        Self::ClaudeCredit,
        Self::Codex5h,
        Self::CodexWeekly,
        Self::CodexCredit,
        Self::Gemini5h,
        Self::GeminiWeekly,
    ];
    pub fn menu_bar_cases() -> Vec<Self> {
        Self::ALL.into_iter().filter(|id| !id.is_credit()).collect()
    }
    pub fn is_credit(self) -> bool {
        matches!(self, Self::ClaudeCredit | Self::CodexCredit)
    }
    pub fn provider(self) -> Option<RateLimitProvider> {
        match self {
            Self::Budget => None,
            Self::Claude5h | Self::ClaudeWeekly | Self::ClaudeCredit => {
                Some(RateLimitProvider::Claude)
            }
            Self::Codex5h | Self::CodexWeekly | Self::CodexCredit => Some(RateLimitProvider::Codex),
            Self::Gemini5h | Self::GeminiWeekly => Some(RateLimitProvider::Gemini),
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct MenuBarStatusEntry {
    pub id: PieChartId,
    pub fraction: Option<f64>,
    pub show_icon: bool,
    pub ratio_label: Option<String>,
    pub is_budget_unset: bool,
    pub is_budget_unknown: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    #[serde(
        rename = "budgetUSD",
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_decimal_number"
    )]
    pub budget_usd: Option<Decimal>,
    #[serde(rename = "budgetMachineIDs")]
    pub budget_machine_ids: Vec<String>,
    pub reset_cycle: ResetCycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<ResetBaseline>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_interval_seconds: Option<u32>,
    pub menu_bar_pie_charts: Vec<PieChartId>,
    pub menu_bar_ratio_labels: Vec<PieChartId>,
    pub menu_bar_show_cost: bool,
    pub menu_bar_cost_only: bool,
}
fn normalize_pie_chart_ids(raw: Vec<String>) -> Vec<PieChartId> {
    let ids: Vec<PieChartId> = raw
        .into_iter()
        .filter_map(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
        .collect();
    PieChartId::ALL
        .into_iter()
        .filter(|id| ids.contains(id))
        .collect()
}
impl<'de> Deserialize<'de> for AppState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(rename = "budgetUSD", default, with = "optional_decimal_number")]
            budget_usd: Option<Decimal>,
            #[serde(rename = "budgetMachineIDs", default)]
            budget_machine_ids: Vec<String>,
            reset_cycle: ResetCycle,
            baseline: Option<ResetBaseline>,
            refresh_interval_seconds: Option<u32>,
            #[serde(default)]
            menu_bar_pie_charts: Vec<String>,
            menu_bar_ratio_labels: Option<Vec<String>>,
            menu_bar_show_cost: Option<bool>,
            #[serde(default)]
            menu_bar_cost_only: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut charts = normalize_pie_chart_ids(wire.menu_bar_pie_charts);
        if charts.is_empty() {
            charts.push(PieChartId::Budget);
        }
        let ratios = wire
            .menu_bar_ratio_labels
            .map(normalize_pie_chart_ids)
            .unwrap_or_else(|| vec![PieChartId::Budget]);
        let show_cost = wire
            .menu_bar_show_cost
            .unwrap_or_else(|| ratios.contains(&PieChartId::Budget));
        Ok(Self {
            budget_usd: wire.budget_usd,
            budget_machine_ids: wire.budget_machine_ids,
            reset_cycle: wire.reset_cycle,
            baseline: wire.baseline,
            refresh_interval_seconds: wire.refresh_interval_seconds,
            menu_bar_pie_charts: charts,
            menu_bar_ratio_labels: ratios,
            menu_bar_show_cost: show_cost,
            menu_bar_cost_only: wire.menu_bar_cost_only,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregationCacheRange {
    pub since: String,
    pub through: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregationCacheJob {
    pub since: String,
    pub through: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregationCachePayload {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cached_from: String,
    pub cached_through: String,
    pub metrics: Vec<MetricRecord>,
    pub sessions: Vec<SessionMetricRecord>,
    pub covered_ranges: Vec<AggregationCacheRange>,
    pub directory_covered_ranges: Vec<AggregationCacheRange>,
    pub antigravity_covered_ranges: Vec<AggregationCacheRange>,
    pub source_configuration_fingerprint: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPartitionDecision {
    Complete,
    PartialOrUnverifiable,
    Absent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardUIState {
    pub range: String,
    pub custom_start: String,
    pub custom_end: String,
    pub selected_models: Vec<String>,
    pub selected_agents: Vec<String>,
    pub selected_machines: Vec<String>,
    pub granularity: String,
    pub chart_metric: String,
    pub stack_by: String,
    pub sidebar_collapsed: bool,
    pub header_collapsed: bool,
    pub pie_charts: Vec<PieChartId>,
    pub hidden_pie_ratios: Vec<PieChartId>,
}
impl<'de> Deserialize<'de> for DashboardUIState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            range: String,
            custom_start: String,
            custom_end: String,
            selected_models: Vec<String>,
            selected_agents: Vec<String>,
            selected_machines: Option<Vec<String>>,
            granularity: String,
            chart_metric: String,
            stack_by: Option<String>,
            sidebar_collapsed: Option<bool>,
            header_collapsed: Option<bool>,
            pie_charts: Option<Vec<String>>,
            hidden_pie_ratios: Option<Vec<String>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let stack_by = wire
            .stack_by
            .filter(|s| ["model", "machine", "subdirectory", "modelEffort"].contains(&s.as_str()))
            .unwrap_or_else(|| "model".to_owned());
        let mut pie_charts = normalize_pie_chart_ids(wire.pie_charts.unwrap_or_default());
        if pie_charts.is_empty() {
            pie_charts.push(PieChartId::Budget);
        }
        Ok(Self {
            range: wire.range,
            custom_start: wire.custom_start,
            custom_end: wire.custom_end,
            selected_models: wire.selected_models,
            selected_agents: wire.selected_agents,
            selected_machines: wire.selected_machines.unwrap_or_default(),
            granularity: wire.granularity,
            chart_metric: wire.chart_metric,
            stack_by,
            sidebar_collapsed: wire.sidebar_collapsed.unwrap_or(false),
            header_collapsed: wire.header_collapsed.unwrap_or(false),
            pie_charts,
            hidden_pie_ratios: normalize_pie_chart_ids(wire.hidden_pie_ratios.unwrap_or_default()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MachineKind {
    Local,
    Ssh,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSessionSources {
    pub codex_session_dirs: Vec<String>,
    pub claude_config_dirs: Vec<String>,
    pub include_default_codex_dir: bool,
    pub include_default_claude_dir: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SSHJumpProxy {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_hosts_file: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SSHProxy {
    Direct,
    Jump(SSHJumpProxy),
    Command { executable: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SSHConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    pub extra_options: Vec<String>,
    pub remote_ccusage_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<SSHProxy>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineDescriptor {
    pub id: String,
    pub display_name: String,
    pub kind: MachineKind,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SSHConnection>,
    #[serde(default)]
    pub codex_session_dirs: Vec<String>,
    #[serde(default)]
    pub claude_config_dirs: Vec<String>,
    #[serde(default = "default_true")]
    pub include_default_codex_dir: bool,
    #[serde(default = "default_true")]
    pub include_default_claude_dir: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineRegistry {
    pub schema_version: u32,
    pub local_session_sources: LocalSessionSources,
    pub machines: Vec<MachineDescriptor>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSessionSourcePlan {
    pub machine_id: String,
    pub codex_roots: Vec<PathBuf>,
    pub claude_roots: Vec<PathBuf>,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MachineCollectionState {
    Disabled,
    NeverCollected,
    Healthy,
    Stale,
    Error,
}
pub type MachineHealthState = MachineCollectionState;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SanitizedCollectionError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}
pub type MachineDiagnostic = SanitizedCollectionError;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineStatusDataGap {
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineStatus {
    pub id: String,
    pub display_name: String,
    pub kind: MachineKind,
    pub enabled: bool,
    pub collection_state: MachineCollectionState,
    pub snapshot_available: bool,
    pub collection_in_progress: bool,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_generated_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    pub consecutive_failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_since: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_since: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<SanitizedCollectionError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hour_data_gap: Option<MachineStatusDataGap>,
    pub refresh_interval_seconds: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineLatestEvent {
    pub machine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_event_at: Option<DateTime<Utc>>,
    pub marker_state: String,
    pub in_last_hour: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_quality: Option<UsageDataQuality>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DashboardDataDisposition {
    Current,
    Historical,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineAvailabilityReason {
    CollectionStale,
    NeverCollected,
    TunnelUnreachable,
    AuthFailed,
    HostKeyVerificationFailed,
    Timeout,
    TransportFailed,
    RemoteCommandFailed,
    InvalidResponse,
    CacheFailed,
    ExecutableUnavailable,
    InsufficientCoverage,
    InternalError,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineAvailability {
    pub machine: String,
    pub available: bool,
    pub unavailable_since: DateTime<Utc>,
    pub reason_code: MachineAvailabilityReason,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineDataGap {
    pub machine: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub reason_code: MachineAvailabilityReason,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardScope {
    pub requested: String,
    pub data_disposition: DashboardDataDisposition,
    pub included_machine_ids: Vec<String>,
    pub stale_machine_ids: Vec<String>,
    pub unavailable_machine_ids: Vec<String>,
    pub excluded_from_current_totals_machine_ids: Vec<String>,
    pub machine_availability: Vec<MachineAvailability>,
    pub last_hour_data_gaps: Vec<MachineDataGap>,
    pub evaluated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardRangeLoadProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_start: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_end: Option<DateTime<Utc>>,
    pub completed: usize,
    pub total: usize,
    pub is_loading: bool,
    pub is_partial: bool,
    pub failed_machine_ids: Vec<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindowSlots {
    pub five_hour: Option<RateLimitWindow>,
    pub weekly: Option<RateLimitWindow>,
    pub other: Vec<RateLimitWindow>,
}
pub trait DirectoryNameLifecycle: Send + Sync {
    fn prepare_delete(&self, machine_id: &str) -> Result<(), String>;
    fn commit_delete(&self, machine_id: &str) -> Result<(), String>;
    fn rollback_delete(&self, machine_id: &str) -> Result<(), String>;
    fn purge_deleted(&self, machine_id: &str) -> Result<(), String>;
    fn purge_for_reuse(&self, machine_id: &str) -> Result<(), String>;
    fn reconcile_machine_registry(&self, machine_ids: &[String]) -> Result<(), String>;
}
pub trait MachineRuntimeReconciler: Send + Sync {
    fn reconcile_registry<'a>(
        &'a self,
        registry: &'a MachineRegistry,
        revision: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}
pub trait PricingHttpTransport: Send + Sync {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;
}
