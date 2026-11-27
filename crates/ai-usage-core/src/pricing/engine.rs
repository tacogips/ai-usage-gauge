use super::{fallback_table, AntigravitySchedule};
use crate::types::{PricedUsageEvent, TimestampedUsageEvent};
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelPricingRates {
    pub input: Decimal,
    pub output: Decimal,
    pub cache_read: Option<Decimal>,
    pub cache_creation_5m: Option<Decimal>,
    pub cache_creation_1h: Option<Decimal>,
    pub input_above_200k: Option<Decimal>,
    pub output_above_200k: Option<Decimal>,
    pub cache_read_above_200k: Option<Decimal>,
    pub cache_creation_above_200k: Option<Decimal>,
}

#[derive(Clone, Debug, Default)]
pub struct PricingEngine {
    pub rates: HashMap<String, ModelPricingRates>,
}
impl PricingEngine {
    pub fn new(rates: HashMap<String, ModelPricingRates>) -> Self {
        Self { rates }
    }
    pub fn price_event(&self, event: &TimestampedUsageEvent) -> PricedUsageEvent {
        let rate = if event.agent == "agy" {
            AntigravitySchedule::rates(&event.model, event.timestamp)
        } else {
            None
        }
        .or_else(|| resolve(&self.rates, &event.model))
        .or_else(|| resolve(&fallback_table::rates(), &event.model))
        .or_else(|| fallback_table::family_fallback(&event.model));
        let cost_usd = rate.map_or(Decimal::ZERO, |r| {
            let five = event.cache_creation_5m_tokens.saturating_add(
                event.cache_creation_tokens.saturating_sub(
                    event
                        .cache_creation_5m_tokens
                        .saturating_add(event.cache_creation_1h_tokens),
                ),
            );
            let read = r.cache_read.unwrap_or(r.input / Decimal::from(10));
            let create = r
                .cache_creation_5m
                .unwrap_or(r.input * Decimal::new(125, 2));
            let hour = r.cache_creation_1h.unwrap_or(r.input * Decimal::from(2));
            bucket(event.input_tokens, r.input, r.input_above_200k)
                + bucket(event.output_tokens, r.output, r.output_above_200k)
                + bucket(event.cache_read_tokens, read, r.cache_read_above_200k)
                + bucket(five, create, r.cache_creation_above_200k)
                + bucket(
                    event.cache_creation_1h_tokens,
                    hour,
                    r.cache_creation_above_200k,
                )
        });
        PricedUsageEvent {
            event: event.clone(),
            cost_usd,
        }
    }
}
fn bucket(n: u64, base: Decimal, upper: Option<Decimal>) -> Decimal {
    Decimal::from(n.min(200_000)) * base
        + Decimal::from(n.saturating_sub(200_000)) * upper.unwrap_or(base)
}
fn resolve(table: &HashMap<String, ModelPricingRates>, model: &str) -> Option<ModelPricingRates> {
    let name = model.to_lowercase();
    let unprefixed = name.split_once('/').map_or(name.as_str(), |(_, tail)| tail);
    for key in [name.as_str(), unprefixed] {
        if let Some(r) = table.get(key) {
            return Some(r.clone());
        }
        for prefix in ["anthropic/", "openai/", "google/", "gemini/"] {
            if let Some(r) = table.get(&format!("{prefix}{key}")) {
                return Some(r.clone());
            }
        }
    }
    let mut stripped = unprefixed.trim_end_matches(":0").to_owned();
    if stripped.len() >= 9
        && stripped.is_char_boundary(stripped.len() - 9)
        && stripped.as_bytes()[stripped.len() - 9] == b'-'
        && stripped[stripped.len() - 8..]
            .bytes()
            .all(|c| c.is_ascii_digit())
    {
        stripped.truncate(stripped.len() - 9);
    } else if stripped.len() >= 11 && stripped.is_char_boundary(stripped.len() - 11) {
        let suffix = &stripped[stripped.len() - 11..];
        if suffix.as_bytes()[0] == b'-'
            && suffix.as_bytes()[5] == b'-'
            && suffix.as_bytes()[8] == b'-'
            && suffix
                .bytes()
                .enumerate()
                .all(|(i, c)| [0, 5, 8].contains(&i) || c.is_ascii_digit())
        {
            stripped.truncate(stripped.len() - 11);
        }
    }
    if stripped != unprefixed {
        return resolve(table, &stripped);
    }
    None
}
