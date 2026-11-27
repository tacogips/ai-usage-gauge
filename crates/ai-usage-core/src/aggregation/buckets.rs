use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;

use crate::types::{MetricRecord, PricedUsageEvent, SessionMetricRecord, UsageDataQuality};

#[derive(Default)]
struct Totals {
    cost: Decimal,
    input: u64,
    output: u64,
    creation: u64,
    read: u64,
}

impl Totals {
    fn add(&mut self, priced: &PricedUsageEvent) -> Result<(), &'static str> {
        let event = &priced.event;
        self.cost = self
            .cost
            .checked_add(priced.cost_usd)
            .ok_or("cost overflow")?;
        self.input = self
            .input
            .checked_add(event.input_tokens)
            .ok_or("token overflow")?;
        self.output = self
            .output
            .checked_add(event.output_tokens)
            .ok_or("token overflow")?;
        self.creation = self
            .creation
            .checked_add(event.cache_creation_tokens)
            .ok_or("token overflow")?;
        self.read = self
            .read
            .checked_add(event.cache_read_tokens)
            .ok_or("token overflow")?;
        Ok(())
    }

    fn total(&self) -> Result<u64, &'static str> {
        self.input
            .checked_add(self.output)
            .and_then(|v| v.checked_add(self.creation))
            .and_then(|v| v.checked_add(self.read))
            .ok_or("token overflow")
    }
}

type SessionKey = (
    DateTime<Utc>,
    String,
    String,
    Option<String>,
    Option<String>,
);
type DailyKey = (String, String, String);

/// Aggregate the same priced events for chart buckets and local-calendar daily totals.
pub fn aggregate_events(
    events: &[PricedUsageEvent],
    tz: Tz,
) -> Result<(Vec<MetricRecord>, Vec<SessionMetricRecord>), &'static str> {
    let mut sessions: BTreeMap<SessionKey, Totals> = BTreeMap::new();
    let mut daily: BTreeMap<DailyKey, Totals> = BTreeMap::new();
    for priced in events {
        let event = &priced.event;
        let seconds = event.timestamp.timestamp().div_euclid(900) * 900;
        let bucket = DateTime::from_timestamp(seconds, 0).ok_or("invalid bucket timestamp")?;
        sessions
            .entry((
                bucket,
                event.agent.clone(),
                event.model.clone(),
                event.directory.clone(),
                event.effort.clone(),
            ))
            .or_default()
            .add(priced)?;
        daily
            .entry((
                event.timestamp.with_timezone(&tz).date_naive().to_string(),
                event.agent.clone(),
                event.model.clone(),
            ))
            .or_default()
            .add(priced)?;
    }
    let metrics = daily
        .into_iter()
        .map(|((date, agent, model), t)| {
            Ok(MetricRecord {
                date,
                agent,
                model,
                cost_usd: t.cost,
                input_tokens: t.input,
                output_tokens: t.output,
                cache_creation_tokens: t.creation,
                cache_read_tokens: t.read,
                total_tokens: t.total()?,
                machine: "local".into(),
                directory: None,
                effort: None,
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    let sessions = sessions
        .into_iter()
        .map(|((timestamp, agent, model, directory, effort), t)| {
            Ok(SessionMetricRecord {
                timestamp,
                agent,
                model,
                cost_usd: t.cost,
                input_tokens: t.input,
                output_tokens: t.output,
                cache_creation_tokens: t.creation,
                cache_read_tokens: t.read,
                total_tokens: t.total()?,
                data_quality: UsageDataQuality::Timestamped,
                machine: "local".into(),
                directory,
                effort,
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    Ok((metrics, sessions))
}
