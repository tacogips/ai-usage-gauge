use super::ModelPricingRates;
use rust_decimal::Decimal;
use std::collections::HashMap;
pub fn rates() -> HashMap<String, ModelPricingRates> {
    let mut result = HashMap::new();
    for (names, input, output) in [
        ("claude-opus-4|claude-opus-4-1|claude-opus-4.1", 1500, 7500),
        (
            "claude-opus-4-5|claude-opus-4-6|claude-opus-4.5|claude-opus-4.6|claude-opus-5|claude-opus-5-5|claude-opus-5.5",
            500,
            2500,
        ),
        (
            "claude-sonnet-4|claude-sonnet-4-5|claude-sonnet-4-6|claude-sonnet-4.5|claude-sonnet-4.6|claude-sonnet-5|claude-sonnet-5-5|claude-sonnet-5.5|claude-3-7-sonnet|claude-fable-5-1|claude-fable-5-2",
            300,
            1500,
        ),
        (
            "claude-3-5-haiku|claude-haiku-4-5|claude-haiku-4.5|claude-haiku-5|claude-haiku-5-5|claude-haiku-5.5",
            100,
            500,
        ),
        ("gpt-5|gpt-5-codex|gpt-5.1|gpt-5.1-codex|gpt-5.2-codex", 125, 1000),
        ("gpt-5.2|gpt-5.3-codex", 175, 1400),
        (
            "gpt-5.4|gpt-5.5|gpt-5.6|gpt-6|gpt-6-luna|gpt-6.1|gpt-6.1-sol|gpt-6.1-terra|gpt-6.2|gpt-6.2-sol|gpt-6.2-astra",
            250,
            1500,
        ),
        ("gpt-4.1", 200, 800),
        ("o3", 200, 800),
        ("o4-mini", 110, 440),
        ("gemini-2.5-pro", 125, 1000),
        ("gemini-2.5-flash", 30, 250),
        ("gemini-2.0-flash", 10, 40),
    ] {
        for name in names.split('|') {
            result.insert(
                name.to_string(),
                ModelPricingRates {
                    input: Decimal::new(input, 8),
                    output: Decimal::new(output, 8),
                    ..Default::default()
                },
            );
        }
    }
    result
}

/// Family-prefix fallback when an exact or date-stripped model ID is not yet
/// in the static/LiteLLM table. Synthetic/unknown markers stay unpriced ($0).
pub fn family_fallback(model: &str) -> Option<ModelPricingRates> {
    let lower = model.to_lowercase();
    let name = lower.split_once('/').map_or(lower.as_str(), |(_, tail)| tail);
    if name.starts_with('<') || name == "unknown" {
        return None;
    }
    let (input, output) = if name.starts_with("claude-opus-") {
        (500, 2500)
    } else if name.starts_with("claude-haiku-") {
        (100, 500)
    } else if name.starts_with("claude-sonnet-") || name.starts_with("claude-fable-") {
        (300, 1500)
    } else if name.starts_with("gpt-6") || name.starts_with("gpt-5") {
        (250, 1500)
    } else {
        return None;
    };
    Some(ModelPricingRates {
        input: Decimal::new(input, 8),
        output: Decimal::new(output, 8),
        ..Default::default()
    })
}
