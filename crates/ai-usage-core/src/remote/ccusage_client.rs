//! Decoding and coalescing results returned by the remote ccusage command.
use crate::types::{CostRecord, MetricRecord, SessionMetricRecord, UsageDataQuality};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidResponse;

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, InvalidResponse> {
    v.get(key).ok_or(InvalidResponse)
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, InvalidResponse> {
    field(v, key)?.as_str().ok_or(InvalidResponse)
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a [Value], InvalidResponse> {
    field(v, key)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or(InvalidResponse)
}
fn decimal(v: &Value, key: &str) -> Result<Decimal, InvalidResponse> {
    let value = field(v, key)?;
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return Err(InvalidResponse),
    };
    Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .map_err(|_| InvalidResponse)
}
fn timestamp(text: &str) -> Result<DateTime<Utc>, InvalidResponse> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| InvalidResponse)
}
fn parse(data: &[u8]) -> Result<Value, InvalidResponse> {
    serde_json::from_slice(data).map_err(|_| InvalidResponse)
}
#[derive(Clone)]
struct Breakdown {
    model: String,
    cost: Decimal,
    input: u64,
    output: u64,
    creation: u64,
    read: u64,
    total: u64,
}
fn breakdown(v: &Value) -> Result<Breakdown, InvalidResponse> {
    let number = |key| field(v, key)?.as_u64().ok_or(InvalidResponse);
    let input: u64 = number("inputTokens")?;
    let output = number("outputTokens")?;
    let creation = number("cacheCreationTokens")?;
    let read = number("cacheReadTokens")?;
    let total = input
        .checked_add(output)
        .and_then(|n| n.checked_add(creation))
        .and_then(|n| n.checked_add(read))
        .ok_or(InvalidResponse)?;
    Ok(Breakdown {
        model: string(v, "modelName")?.into(),
        cost: decimal(v, "cost")?,
        input,
        output,
        creation,
        read,
        total,
    })
}

/// Decoder and daily argument compatibility state for one remote machine.
#[derive(Debug, Default)]
pub struct RemoteCCUsageClient {
    by_agent: Option<bool>,
}
impl RemoteCCUsageClient {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn decode_blocks(data: &[u8], machine: &str) -> Result<Vec<CostRecord>, InvalidResponse> {
        let v = parse(data)?;
        array(&v, "blocks")?
            .iter()
            .map(|block| {
                let models = array(block, "models")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned).ok_or(InvalidResponse))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(CostRecord {
                    timestamp: timestamp(string(block, "startTime")?)?,
                    cost_usd: decimal(block, "costUSD")?,
                    models,
                    machine: machine.into(),
                    directory: None,
                })
            })
            .collect()
    }
    pub fn decode_daily(data: &[u8], machine: &str) -> Result<Vec<MetricRecord>, InvalidResponse> {
        let v = parse(data)?;
        let mut result = Vec::new();
        for day in array(&v, "daily")? {
            let date = string(day, "period")?;
            for agent in array(day, "agents")? {
                let name = string(agent, "agent")?;
                for model in array(agent, "modelBreakdowns")? {
                    let b = breakdown(model)?;
                    result.push(MetricRecord {
                        date: date.into(),
                        agent: name.into(),
                        model: b.model,
                        cost_usd: b.cost,
                        input_tokens: b.input,
                        output_tokens: b.output,
                        cache_creation_tokens: b.creation,
                        cache_read_tokens: b.read,
                        total_tokens: b.total,
                        machine: machine.into(),
                        directory: None,
                        effort: None,
                    });
                }
            }
        }
        Ok(result)
    }
    pub fn decode_sessions(
        data: &[u8],
        machine: &str,
    ) -> Result<Vec<SessionMetricRecord>, InvalidResponse> {
        let v = parse(data)?;
        let mut result = Vec::new();
        for session in array(&v, "session")? {
            let time = timestamp(string(field(session, "metadata")?, "lastActivity")?)?;
            let agent = string(session, "agent")?;
            for model in array(session, "modelBreakdowns")? {
                let b = breakdown(model)?;
                result.push(SessionMetricRecord {
                    timestamp: time,
                    agent: agent.into(),
                    model: b.model,
                    cost_usd: b.cost,
                    input_tokens: b.input,
                    output_tokens: b.output,
                    cache_creation_tokens: b.creation,
                    cache_read_tokens: b.read,
                    total_tokens: b.total,
                    data_quality: UsageDataQuality::SessionEstimated,
                    machine: machine.into(),
                    directory: None,
                    effort: None,
                });
            }
        }
        Ok(result)
    }
    /// `fetch` receives true when `--by-agent` must be supplied. Empty responses
    /// do not decide compatibility, since they contain no agent fields to inspect.
    pub fn daily<F>(
        &mut self,
        machine: &str,
        mut fetch: F,
    ) -> Result<Vec<MetricRecord>, InvalidResponse>
    where
        F: FnMut(bool) -> Result<Vec<u8>, InvalidResponse>,
    {
        let mode = self.by_agent.unwrap_or(false);
        let first = Self::decode_daily(&fetch(mode)?, machine);
        let (result, used_mode) = match first {
            Ok(rows) => (rows, mode),
            Err(_) if self.by_agent.is_none() => {
                (Self::decode_daily(&fetch(true)?, machine)?, true)
            }
            Err(e) => return Err(e),
        };
        if !result.is_empty() {
            self.by_agent = Some(used_mode);
        }
        Ok(result)
    }
}

/// Coalesce independent source outputs before stamping the machine identifier.
pub fn coalesce_points(
    rows: Vec<CostRecord>,
    machine: &str,
) -> Result<Vec<CostRecord>, InvalidResponse> {
    let mut result: Vec<CostRecord> = Vec::new();
    for mut row in rows {
        if let Some(existing) = result
            .iter_mut()
            .find(|v| v.timestamp == row.timestamp && v.directory == row.directory)
        {
            existing.cost_usd = existing
                .cost_usd
                .checked_add(row.cost_usd)
                .ok_or(InvalidResponse)?;
            for model in row.models {
                if !existing.models.contains(&model) {
                    existing.models.push(model);
                }
            }
        } else {
            row.machine = machine.into();
            result.push(row);
        }
    }
    for row in &mut result {
        row.models.sort();
        row.models.dedup();
    }
    result.sort_by(|a, b| (a.timestamp, &a.directory).cmp(&(b.timestamp, &b.directory)));
    Ok(result)
}

pub fn coalesce_metrics(
    rows: Vec<MetricRecord>,
    machine: &str,
) -> Result<Vec<MetricRecord>, InvalidResponse> {
    let mut result: Vec<MetricRecord> = Vec::new();
    for mut row in rows {
        if let Some(existing) = result.iter_mut().find(|v| {
            v.date == row.date
                && v.agent == row.agent
                && v.model == row.model
                && v.directory == row.directory
                && v.effort == row.effort
        }) {
            existing.cost_usd = existing
                .cost_usd
                .checked_add(row.cost_usd)
                .ok_or(InvalidResponse)?;
            existing.input_tokens = existing
                .input_tokens
                .checked_add(row.input_tokens)
                .ok_or(InvalidResponse)?;
            existing.output_tokens = existing
                .output_tokens
                .checked_add(row.output_tokens)
                .ok_or(InvalidResponse)?;
            existing.cache_creation_tokens = existing
                .cache_creation_tokens
                .checked_add(row.cache_creation_tokens)
                .ok_or(InvalidResponse)?;
            existing.cache_read_tokens = existing
                .cache_read_tokens
                .checked_add(row.cache_read_tokens)
                .ok_or(InvalidResponse)?;
            existing.total_tokens = existing
                .input_tokens
                .checked_add(existing.output_tokens)
                .and_then(|n| n.checked_add(existing.cache_creation_tokens))
                .and_then(|n| n.checked_add(existing.cache_read_tokens))
                .ok_or(InvalidResponse)?;
        } else {
            row.machine = machine.into();
            result.push(row);
        }
    }
    result.sort_by(|a, b| {
        (&a.date, &a.agent, &a.model, &a.directory, &a.effort).cmp(&(
            &b.date,
            &b.agent,
            &b.model,
            &b.directory,
            &b.effort,
        ))
    });
    Ok(result)
}

pub fn coalesce_sessions(
    rows: Vec<SessionMetricRecord>,
    machine: &str,
) -> Result<Vec<SessionMetricRecord>, InvalidResponse> {
    let mut result: Vec<SessionMetricRecord> = Vec::new();
    for mut row in rows {
        if let Some(existing) = result.iter_mut().find(|v| {
            v.timestamp == row.timestamp
                && v.agent == row.agent
                && v.model == row.model
                && v.directory == row.directory
                && v.effort == row.effort
                && v.data_quality == row.data_quality
        }) {
            existing.cost_usd = existing
                .cost_usd
                .checked_add(row.cost_usd)
                .ok_or(InvalidResponse)?;
            existing.input_tokens = existing
                .input_tokens
                .checked_add(row.input_tokens)
                .ok_or(InvalidResponse)?;
            existing.output_tokens = existing
                .output_tokens
                .checked_add(row.output_tokens)
                .ok_or(InvalidResponse)?;
            existing.cache_creation_tokens = existing
                .cache_creation_tokens
                .checked_add(row.cache_creation_tokens)
                .ok_or(InvalidResponse)?;
            existing.cache_read_tokens = existing
                .cache_read_tokens
                .checked_add(row.cache_read_tokens)
                .ok_or(InvalidResponse)?;
            existing.total_tokens = existing
                .input_tokens
                .checked_add(existing.output_tokens)
                .and_then(|n| n.checked_add(existing.cache_creation_tokens))
                .and_then(|n| n.checked_add(existing.cache_read_tokens))
                .ok_or(InvalidResponse)?;
        } else {
            row.machine = machine.into();
            result.push(row);
        }
    }
    result.sort_by(|a, b| {
        (
            a.timestamp,
            &a.agent,
            &a.model,
            &a.directory,
            &a.effort,
            matches!(a.data_quality, UsageDataQuality::SessionEstimated),
        )
            .cmp(&(
                b.timestamp,
                &b.agent,
                &b.model,
                &b.directory,
                &b.effort,
                matches!(b.data_quality, UsageDataQuality::SessionEstimated),
            ))
    });
    Ok(result)
}
