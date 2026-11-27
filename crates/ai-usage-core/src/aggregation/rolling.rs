use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;

use crate::types::{MetricRecord, PricedUsageEvent, ResetCycle, SessionMetricRecord};

/// Sum exact timestamps in a half-open interval, independently of chart buckets.
pub fn exact_event_cost(
    events: &[PricedUsageEvent],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    directories: &[String],
) -> Decimal {
    events
        .iter()
        .filter(|priced| {
            let event = &priced.event;
            event.timestamp >= start
                && event.timestamp < end
                && (directories.is_empty()
                    || event
                        .directory
                        .as_ref()
                        .is_some_and(|d| directories.contains(d)))
        })
        .map(|priced| priced.cost_usd)
        .sum()
}

/// Input for one local calendar-day, agent, and model partition.
/// The caller supplies rows for exactly that partition, including every raw event
/// in the day, so that the legacy completeness and cost allocation are valid.
pub struct RollingPartition<'a> {
    pub raw: &'a [PricedUsageEvent],
    pub cached_daily: &'a [MetricRecord],
    pub cached_sessions: &'a [SessionMetricRecord],
    pub native_or_today: bool,
}

/// Select one authoritative input for a partition and apply the same interval
/// and directory predicate to either exact events or retained legacy sessions.
pub fn partition_cost(
    partition: RollingPartition<'_>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    directories: &[String],
) -> Result<Decimal, &'static str> {
    use crate::cache::evaluate_legacy_partition_completeness;
    use crate::types::{LegacyPartitionDecision, UsageDataQuality};

    let matches = |timestamp: DateTime<Utc>, directory: &Option<String>| {
        timestamp >= start
            && timestamp < end
            && (directories.is_empty()
                || directory.as_ref().is_some_and(|d| directories.contains(d)))
    };
    if partition.native_or_today {
        return partition
            .raw
            .iter()
            .filter(|p| matches(p.event.timestamp, &p.event.directory))
            .try_fold(Decimal::ZERO, |sum, p| {
                sum.checked_add(p.cost_usd).ok_or("cost overflow")
            });
    }
    let mut scanned = Vec::with_capacity(partition.raw.len());
    for p in partition.raw {
        let e = &p.event;
        let total = e
            .input_tokens
            .checked_add(e.output_tokens)
            .and_then(|v| v.checked_add(e.cache_creation_tokens))
            .and_then(|v| v.checked_add(e.cache_read_tokens))
            .ok_or("token overflow")?;
        scanned.push(SessionMetricRecord {
            timestamp: e.timestamp,
            agent: e.agent.clone(),
            model: e.model.clone(),
            cost_usd: p.cost_usd,
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            cache_creation_tokens: e.cache_creation_tokens,
            cache_read_tokens: e.cache_read_tokens,
            total_tokens: total,
            data_quality: UsageDataQuality::Timestamped,
            machine: "local".into(),
            directory: e.directory.clone(),
            effort: e.effort.clone(),
        });
    }
    if evaluate_legacy_partition_completeness(partition.cached_daily, &scanned)
        != LegacyPartitionDecision::Complete
    {
        return partition
            .cached_sessions
            .iter()
            .filter(|r| matches(r.timestamp, &r.directory))
            .try_fold(Decimal::ZERO, |sum, r| {
                sum.checked_add(r.cost_usd).ok_or("cost overflow")
            });
    }
    let cached_cost = partition
        .cached_daily
        .iter()
        .try_fold(Decimal::ZERO, |sum, r| {
            sum.checked_add(r.cost_usd).ok_or("cost overflow")
        })?;
    let raw_cost = partition.raw.iter().try_fold(Decimal::ZERO, |sum, p| {
        sum.checked_add(p.cost_usd).ok_or("cost overflow")
    })?;
    let mut allocated = Decimal::ZERO;
    let mut selected = Decimal::ZERO;
    for (index, p) in partition.raw.iter().enumerate() {
        let cost = if index + 1 == partition.raw.len() {
            cached_cost.checked_sub(allocated).ok_or("cost overflow")?
        } else if raw_cost.is_zero() {
            Decimal::ZERO
        } else {
            cached_cost
                .checked_mul(p.cost_usd)
                .and_then(|v| v.checked_div(raw_cost))
                .ok_or("cost overflow")?
        };
        allocated = allocated.checked_add(cost).ok_or("cost overflow")?;
        if matches(p.event.timestamp, &p.event.directory) {
            selected = selected.checked_add(cost).ok_or("cost overflow")?;
        }
    }
    Ok(selected)
}

/// Complete inputs for `selected_period_cost`, shared identically by tray, CLI, and `/api/budget`.
#[derive(Debug, Clone, Default)]
pub struct RollingInputs {
    /// Local unfloored priced events for `[retention_floor, today]` and (if applicable) pinned `start_day`.
    pub local_raw_events: Vec<PricedUsageEvent>,
    /// Daily metric records from SQLite cache + today.
    pub daily_metrics: Vec<MetricRecord>,
    /// Session metric records from SQLite cache + today (both local and remote machines).
    pub session_metrics: Vec<SessionMetricRecord>,
    /// Local calendar days (`YYYY-MM-DD`) that originated from Swift/legacy cache and require
    /// authoritative legacy completeness checks.
    pub legacy_local_days: BTreeSet<String>,
    /// Current local calendar day (`YYYY-MM-DD`).
    pub today: String,
    /// Scan retention floor (`YYYY-MM-DD`); interior days strictly before this use cached totals.
    pub retention_floor: String,
}

/// Borrowed inputs for `selected_period_cost_ref`, avoiding snapshot vector clones on hot paths.
pub struct RollingInputsRef<'a> {
    pub local_raw_events: &'a [PricedUsageEvent],
    pub daily_metrics: &'a [MetricRecord],
    pub session_metrics: &'a [SessionMetricRecord],
    pub legacy_local_days: &'a BTreeSet<String>,
    pub today: &'a str,
    pub retention_floor: &'a str,
}

/// Compute the exact cost for `(cycle, interval)` using unified per-partition precedence.
pub fn selected_period_cost(
    cycle: &ResetCycle,
    interval: (DateTime<Utc>, DateTime<Utc>),
    inputs: &RollingInputs,
    directories: &[String],
    tz: Tz,
) -> Result<Decimal, &'static str> {
    let borrowed = RollingInputsRef {
        local_raw_events: &inputs.local_raw_events,
        daily_metrics: &inputs.daily_metrics,
        session_metrics: &inputs.session_metrics,
        legacy_local_days: &inputs.legacy_local_days,
        today: &inputs.today,
        retention_floor: &inputs.retention_floor,
    };
    selected_period_cost_ref(cycle, interval, &borrowed, directories, tz)
}

/// Zero-copy-input variant of `selected_period_cost` that also prunes rows outside
/// `[start_day, end_inclusive_day]` before grouping.
pub fn selected_period_cost_ref(
    cycle: &ResetCycle,
    interval: (DateTime<Utc>, DateTime<Utc>),
    inputs: &RollingInputsRef<'_>,
    directories: &[String],
    tz: Tz,
) -> Result<Decimal, &'static str> {
    let (start, end) = interval;
    if start >= end {
        return Ok(Decimal::ZERO);
    }

    let start_day = start.with_timezone(&tz).date_naive();
    let end_inclusive_day = (end - chrono::Duration::nanoseconds(1))
        .with_timezone(&tz)
        .date_naive();
    let start_day_str = start_day.format("%Y-%m-%d").to_string();
    let end_day_str = end_inclusive_day.format("%Y-%m-%d").to_string();

    let mut total = Decimal::ZERO;

    // 1. Remote machine sessions: always filtered by [start, end) and directory.
    for row in inputs
        .session_metrics
        .iter()
        .filter(|r| r.machine != "local")
    {
        if row.timestamp >= start
            && row.timestamp < end
            && (directories.is_empty()
                || row
                    .directory
                    .as_ref()
                    .is_some_and(|d| directories.contains(d)))
        {
            total = total.checked_add(row.cost_usd).ok_or("cost overflow")?;
        }
    }

    // 2. Local machine: group only rows within [start_day, end_inclusive_day] by (local_day, agent, model).
    let mut raw_by_day_part: BTreeMap<(String, String, String), Vec<PricedUsageEvent>> =
        BTreeMap::new();
    for ev in inputs.local_raw_events {
        let local_date = ev.event.timestamp.with_timezone(&tz).date_naive();
        if local_date < start_day || local_date > end_inclusive_day {
            continue;
        }
        let day = local_date.format("%Y-%m-%d").to_string();
        raw_by_day_part
            .entry((day, ev.event.agent.clone(), ev.event.model.clone()))
            .or_default()
            .push(ev.clone());
    }
    let mut daily_by_day_part: BTreeMap<(String, String, String), Vec<MetricRecord>> =
        BTreeMap::new();
    for row in inputs.daily_metrics.iter().filter(|r| {
        r.machine == "local"
            && r.date.as_str() >= start_day_str.as_str()
            && r.date.as_str() <= end_day_str.as_str()
    }) {
        daily_by_day_part
            .entry((row.date.clone(), row.agent.clone(), row.model.clone()))
            .or_default()
            .push(row.clone());
    }
    let mut sess_by_day_part: BTreeMap<(String, String, String), Vec<SessionMetricRecord>> =
        BTreeMap::new();
    for row in inputs
        .session_metrics
        .iter()
        .filter(|r| r.machine == "local")
    {
        let local_date = row.timestamp.with_timezone(&tz).date_naive();
        if local_date < start_day || local_date > end_inclusive_day {
            continue;
        }
        let day = local_date.format("%Y-%m-%d").to_string();
        sess_by_day_part
            .entry((day, row.agent.clone(), row.model.clone()))
            .or_default()
            .push(row.clone());
    }

    let mut day = start_day;
    while day <= end_inclusive_day {
        let day_str = day.format("%Y-%m-%d").to_string();
        let is_today = day_str.as_str() == inputs.today;
        let is_interior_full_day = matches!(cycle, ResetCycle::CustomHours(_))
            && day > start_day
            && day < end_inclusive_day
            && day_str.as_str() < inputs.retention_floor;

        if is_interior_full_day {
            // Interior completed day strictly between start_day and retention_floor:
            // reuse cached totals without reading raw logs.
            if directories.is_empty() {
                for ((d, _, _), rows) in &daily_by_day_part {
                    if d == &day_str {
                        for row in rows {
                            total = total.checked_add(row.cost_usd).ok_or("cost overflow")?;
                        }
                    }
                }
            } else {
                for ((d, _, _), rows) in &sess_by_day_part {
                    if d == &day_str {
                        for row in rows {
                            if row
                                .directory
                                .as_ref()
                                .is_some_and(|dir| directories.contains(dir))
                            {
                                total = total.checked_add(row.cost_usd).ok_or("cost overflow")?;
                            }
                        }
                    }
                }
            }
        } else {
            // Collect all (agent, model) partitions present in raw, daily, or sessions for this day.
            let mut keys: BTreeSet<(String, String)> = BTreeSet::new();
            for (d, a, m) in raw_by_day_part.keys() {
                if d == &day_str {
                    keys.insert((a.clone(), m.clone()));
                }
            }
            for (d, a, m) in daily_by_day_part.keys() {
                if d == &day_str {
                    keys.insert((a.clone(), m.clone()));
                }
            }
            for (d, a, m) in sess_by_day_part.keys() {
                if d == &day_str {
                    keys.insert((a.clone(), m.clone()));
                }
            }
            let is_legacy_day = !is_today && inputs.legacy_local_days.contains(&day_str);
            for (agent, model) in keys {
                let key = (day_str.clone(), agent, model);
                let raw = raw_by_day_part.get(&key).map(Vec::as_slice).unwrap_or(&[]);
                let daily = daily_by_day_part
                    .get(&key)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let sessions = sess_by_day_part.get(&key).map(Vec::as_slice).unwrap_or(&[]);

                let part_cost = if is_today || (!is_legacy_day && !raw.is_empty()) {
                    partition_cost(
                        RollingPartition {
                            raw,
                            cached_daily: daily,
                            cached_sessions: sessions,
                            native_or_today: true,
                        },
                        start,
                        end,
                        directories,
                    )?
                } else if !is_legacy_day && raw.is_empty() {
                    // Non-legacy historical day with no raw events loaded (e.g. older cached day):
                    // sum cached sessions in [start, end) matching directory.
                    sessions
                        .iter()
                        .filter(|r| {
                            r.timestamp >= start
                                && r.timestamp < end
                                && (directories.is_empty()
                                    || r.directory
                                        .as_ref()
                                        .is_some_and(|dir| directories.contains(dir)))
                        })
                        .try_fold(Decimal::ZERO, |sum, r| {
                            sum.checked_add(r.cost_usd).ok_or("cost overflow")
                        })?
                } else {
                    partition_cost(
                        RollingPartition {
                            raw,
                            cached_daily: daily,
                            cached_sessions: sessions,
                            native_or_today: false,
                        },
                        start,
                        end,
                        directories,
                    )?
                };
                total = total.checked_add(part_cost).ok_or("cost overflow")?;
            }
        }

        day = match day.succ_opt() {
            Some(next) => next,
            None => break,
        };
    }

    Ok(total)
}

pub fn days_between(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        out.push(d);
        match d.succ_opt() {
            Some(next) => d = next,
            None => break,
        }
    }
    out
}
