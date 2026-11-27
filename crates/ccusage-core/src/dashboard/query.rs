use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::aggregation::blocks::local_midnight_utc;
use crate::aggregation::rolling::{selected_period_cost, RollingInputs};
use crate::state::ResetWindowCalculator;
use crate::types::{
    AppState, BudgetSummary, CostRecord, CostSnapshot, MetricRecord, ResetCycle,
    SessionMetricRecord,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointsResponse {
    pub generated_at: DateTime<Utc>,
    pub points: Vec<CostRecord>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsResponse {
    pub generated_at: DateTime<Utc>,
    pub total_cost_usd: Decimal,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub records: Vec<MetricRecord>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSeriesPoint {
    pub timestamp: DateTime<Utc>,
    pub date: String,
    pub machine: String,
    pub agent: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(rename = "costUSD")]
    pub cost_usd: Decimal,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSeriesResponse {
    pub generated_at: DateTime<Utc>,
    pub granularity: String,
    pub points: Vec<CostSeriesPoint>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetApiResponse {
    pub generated_at: DateTime<Utc>,
    pub active_boundary_at: DateTime<Utc>,
    pub reset_cycle: ResetCycle,
    pub budget: BudgetSummary,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryFilter {
    /// Map of machine_id -> set of directory paths.
    pub by_machine: BTreeMap<String, BTreeSet<String>>,
}

impl DirectoryFilter {
    pub fn is_empty(&self) -> bool {
        self.by_machine.is_empty()
    }

    pub fn matches(&self, machine: &str, directory: Option<&str>) -> bool {
        if self.by_machine.is_empty() {
            return true;
        }
        let Some(dirs) = self.by_machine.get(machine) else {
            return false;
        };
        directory.is_some_and(|d| dirs.contains(d))
    }

    pub fn local_directories(&self) -> Vec<String> {
        self.by_machine
            .get("local")
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }
}

pub struct DashboardQueryService;

impl DashboardQueryService {
    pub fn recent(snapshot: &CostSnapshot, hours: i64, now: DateTime<Utc>) -> PointsResponse {
        let cutoff = now - Duration::hours(hours.max(1));
        let points = snapshot
            .points
            .iter()
            .filter(|p| p.timestamp >= cutoff)
            .cloned()
            .collect();
        PointsResponse {
            generated_at: snapshot.generated_at,
            points,
        }
    }

    pub fn day(snapshot: &CostSnapshot, day: NaiveDate, tz: Tz) -> PointsResponse {
        let start = local_midnight_utc(day, tz);
        let end = local_midnight_utc(day.succ_opt().unwrap_or(day), tz);
        let points = snapshot
            .points
            .iter()
            .filter(|p| p.timestamp >= start && p.timestamp < end)
            .cloned()
            .collect();
        PointsResponse {
            generated_at: snapshot.generated_at,
            points,
        }
    }

    pub fn period(
        snapshot: &CostSnapshot,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> PointsResponse {
        let points = snapshot
            .points
            .iter()
            .filter(|p| p.timestamp >= start && p.timestamp < end)
            .cloned()
            .collect();
        PointsResponse {
            generated_at: snapshot.generated_at,
            points,
        }
    }

    pub fn metrics(
        snapshot: &CostSnapshot,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        use_sessions: bool,
        dir_filter: &DirectoryFilter,
        tz: Tz,
    ) -> MetricsResponse {
        let records: Vec<MetricRecord> = if use_sessions || !dir_filter.is_empty() {
            let mut grouped: BTreeMap<(String, String, String, String), MetricRecord> =
                BTreeMap::new();
            for s in &snapshot.dashboard_sessions {
                if s.timestamp < start || s.timestamp >= end {
                    continue;
                }
                if !dir_filter.matches(&s.machine, s.directory.as_deref()) {
                    continue;
                }
                let date = s
                    .timestamp
                    .with_timezone(&tz)
                    .format("%Y-%m-%d")
                    .to_string();
                let key = (
                    date.clone(),
                    s.machine.clone(),
                    s.agent.clone(),
                    s.model.clone(),
                );
                let entry = grouped.entry(key).or_insert_with(|| MetricRecord {
                    date,
                    agent: s.agent.clone(),
                    model: s.model.clone(),
                    cost_usd: Decimal::ZERO,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    total_tokens: 0,
                    machine: s.machine.clone(),
                    directory: None,
                    effort: None,
                });
                entry.cost_usd += s.cost_usd;
                entry.input_tokens = entry.input_tokens.saturating_add(s.input_tokens);
                entry.output_tokens = entry.output_tokens.saturating_add(s.output_tokens);
                entry.cache_creation_tokens = entry
                    .cache_creation_tokens
                    .saturating_add(s.cache_creation_tokens);
                entry.cache_read_tokens =
                    entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                entry.total_tokens = entry.total_tokens.saturating_add(s.total_tokens);
            }
            grouped.into_values().collect()
        } else {
            let start_day = start.with_timezone(&tz).format("%Y-%m-%d").to_string();
            let end_day = (end - Duration::nanoseconds(1))
                .with_timezone(&tz)
                .format("%Y-%m-%d")
                .to_string();
            snapshot
                .dashboard_metrics
                .iter()
                .filter(|m| m.date >= start_day && m.date <= end_day)
                .cloned()
                .collect()
        };

        let mut total_cost = Decimal::ZERO;
        let mut input = 0u64;
        let mut output = 0u64;
        let mut creation = 0u64;
        let mut read = 0u64;
        for r in &records {
            total_cost += r.cost_usd;
            input = input.saturating_add(r.input_tokens);
            output = output.saturating_add(r.output_tokens);
            creation = creation.saturating_add(r.cache_creation_tokens);
            read = read.saturating_add(r.cache_read_tokens);
        }

        MetricsResponse {
            generated_at: snapshot.generated_at,
            total_cost_usd: total_cost,
            total_tokens: input
                .saturating_add(output)
                .saturating_add(creation)
                .saturating_add(read),
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: creation,
            cache_read_tokens: read,
            records,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cost_series(
        snapshot: &CostSnapshot,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
        directory_breakdown: bool,
        effort_breakdown: bool,
        dir_filter: &DirectoryFilter,
        tz: Tz,
    ) -> CostSeriesResponse {
        let bucket_seconds: Option<i64> = match granularity {
            "15min" => Some(900),
            "hourly" => Some(3600),
            "6hour" => Some(21_600),
            _ => None,
        };

        type SeriesKey = (
            DateTime<Utc>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        );
        let mut grouped: BTreeMap<SeriesKey, CostSeriesPoint> = BTreeMap::new();

        let use_sessions = bucket_seconds.is_some()
            || directory_breakdown
            || effort_breakdown
            || !dir_filter.is_empty();

        if use_sessions {
            for s in &snapshot.dashboard_sessions {
                if s.timestamp < start || s.timestamp >= end {
                    continue;
                }
                if !dir_filter.matches(&s.machine, s.directory.as_deref()) {
                    continue;
                }
                let date = s
                    .timestamp
                    .with_timezone(&tz)
                    .format("%Y-%m-%d")
                    .to_string();
                let ts = if let Some(sec) = bucket_seconds {
                    let floored = s.timestamp.timestamp().div_euclid(sec) * sec;
                    DateTime::from_timestamp(floored, 0).unwrap_or(s.timestamp)
                } else {
                    let d = s.timestamp.with_timezone(&tz).date_naive();
                    local_midnight_utc(d, tz)
                };
                let dir = if directory_breakdown {
                    s.directory.clone()
                } else {
                    None
                };
                let eff = if effort_breakdown {
                    s.effort.clone()
                } else {
                    None
                };
                let key = (
                    ts,
                    date.clone(),
                    s.machine.clone(),
                    s.agent.clone(),
                    s.model.clone(),
                    dir.clone(),
                    eff.clone(),
                );
                let entry = grouped.entry(key).or_insert_with(|| CostSeriesPoint {
                    timestamp: ts,
                    date,
                    machine: s.machine.clone(),
                    agent: s.agent.clone(),
                    model: s.model.clone(),
                    directory: dir,
                    effort: eff,
                    cost_usd: Decimal::ZERO,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    total_tokens: 0,
                });
                entry.cost_usd += s.cost_usd;
                entry.input_tokens = entry.input_tokens.saturating_add(s.input_tokens);
                entry.output_tokens = entry.output_tokens.saturating_add(s.output_tokens);
                entry.cache_creation_tokens = entry
                    .cache_creation_tokens
                    .saturating_add(s.cache_creation_tokens);
                entry.cache_read_tokens =
                    entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                entry.total_tokens = entry.total_tokens.saturating_add(s.total_tokens);
            }
        } else {
            let start_day = start.with_timezone(&tz).format("%Y-%m-%d").to_string();
            let end_day = (end - Duration::nanoseconds(1))
                .with_timezone(&tz)
                .format("%Y-%m-%d")
                .to_string();
            for m in &snapshot.dashboard_metrics {
                if m.date < start_day || m.date > end_day {
                    continue;
                }
                let Ok(d) = NaiveDate::parse_from_str(&m.date, "%Y-%m-%d") else {
                    continue;
                };
                let ts = local_midnight_utc(d, tz);
                let key = (
                    ts,
                    m.date.clone(),
                    m.machine.clone(),
                    m.agent.clone(),
                    m.model.clone(),
                    None,
                    None,
                );
                let entry = grouped.entry(key).or_insert_with(|| CostSeriesPoint {
                    timestamp: ts,
                    date: m.date.clone(),
                    machine: m.machine.clone(),
                    agent: m.agent.clone(),
                    model: m.model.clone(),
                    directory: None,
                    effort: None,
                    cost_usd: Decimal::ZERO,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    total_tokens: 0,
                });
                entry.cost_usd += m.cost_usd;
                entry.input_tokens = entry.input_tokens.saturating_add(m.input_tokens);
                entry.output_tokens = entry.output_tokens.saturating_add(m.output_tokens);
                entry.cache_creation_tokens = entry
                    .cache_creation_tokens
                    .saturating_add(m.cache_creation_tokens);
                entry.cache_read_tokens =
                    entry.cache_read_tokens.saturating_add(m.cache_read_tokens);
                entry.total_tokens = entry.total_tokens.saturating_add(m.total_tokens);
            }
        }

        CostSeriesResponse {
            generated_at: snapshot.generated_at,
            granularity: granularity.to_owned(),
            points: grouped.into_values().collect(),
        }
    }

    pub fn budget(
        snapshot: &CostSnapshot,
        state: &AppState,
        dir_filter: &DirectoryFilter,
        legacy_local_days: &BTreeSet<String>,
        now: DateTime<Utc>,
        tz: Tz,
    ) -> BudgetApiResponse {
        let calc = ResetWindowCalculator::new(tz);
        let interval = calc
            .aggregation_interval(state.reset_cycle.clone(), now)
            .unwrap_or((snapshot.active_boundary_at, now));
        let today = now.with_timezone(&tz).date_naive();
        let floor = today - Duration::days(8);

        let local_dirs = dir_filter.local_directories();
        // If directory filter is active, filter remote sessions by machine-specific directory set.
        let filtered_sessions: Vec<SessionMetricRecord> = if dir_filter.is_empty() {
            snapshot.dashboard_sessions.clone()
        } else {
            snapshot
                .dashboard_sessions
                .iter()
                .filter(|s| {
                    if s.machine == "local" {
                        true
                    } else {
                        dir_filter.matches(&s.machine, s.directory.as_deref())
                    }
                })
                .cloned()
                .collect()
        };

        // If directory filter is active and contains NO local machine entries, local contributes $0.
        let include_local = dir_filter.is_empty() || dir_filter.by_machine.contains_key("local");
        let inputs = RollingInputs {
            local_raw_events: if include_local {
                snapshot.rolling_events.clone()
            } else {
                vec![]
            },
            daily_metrics: if include_local {
                snapshot.dashboard_metrics.clone()
            } else {
                vec![]
            },
            session_metrics: if include_local {
                filtered_sessions
            } else {
                filtered_sessions
                    .into_iter()
                    .filter(|s| s.machine != "local")
                    .collect()
            },
            legacy_local_days: legacy_local_days.clone(),
            today: today.format("%Y-%m-%d").to_string(),
            retention_floor: floor.format("%Y-%m-%d").to_string(),
        };

        let spent = selected_period_cost(&state.reset_cycle, interval, &inputs, &local_dirs, tz)
            .unwrap_or(Decimal::ZERO);

        BudgetApiResponse {
            generated_at: snapshot.generated_at,
            active_boundary_at: interval.0,
            reset_cycle: state.reset_cycle.clone(),
            budget: BudgetSummary::new(spent, state.budget_usd),
        }
    }
}
