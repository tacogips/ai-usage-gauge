use super::ModelPricingRates;
use rust_decimal::Decimal;
use std::collections::HashMap;
pub fn rates() -> HashMap<String, ModelPricingRates> {
    let mut result = HashMap::new();
    for (names, input, output) in [
        ("claude-opus-4|claude-opus-4-1|claude-opus-4.1", 1500, 7500),
        ("claude-opus-4-5|claude-opus-4-6|claude-opus-4.5|claude-opus-4.6", 500, 2500),
        ("claude-sonnet-4|claude-sonnet-4-5|claude-sonnet-4-6|claude-sonnet-4.5|claude-sonnet-4.6|claude-3-7-sonnet", 300, 1500),
        ("claude-3-5-haiku|claude-haiku-4-5|claude-haiku-4.5", 100, 500),
        ("gpt-5|gpt-5-codex|gpt-5.1|gpt-5.1-codex|gpt-5.2-codex", 125, 1000),
        ("gpt-5.2|gpt-5.3-codex", 175, 1400),
        ("gpt-5.4", 250, 1500),
        ("gpt-4.1", 200, 800), ("o3", 200, 800), ("o4-mini", 110, 440),
        ("gemini-2.5-pro", 125, 1000), ("gemini-2.5-flash", 30, 250), ("gemini-2.0-flash", 10, 40),
    ] {
        for name in names.split('|') { result.insert(name.to_string(), ModelPricingRates { input: Decimal::new(input, 8), output: Decimal::new(output, 8), ..Default::default() }); }
    }
    result
}
