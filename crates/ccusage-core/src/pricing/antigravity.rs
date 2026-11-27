use super::ModelPricingRates;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
pub struct AntigravitySchedule;
impl AntigravitySchedule {
    pub fn rates(model: &str, date: DateTime<Utc>) -> Option<ModelPricingRates> {
        let (input, output, read) = match model.to_lowercase().as_str() {
            "gemini-4-neon" => (3000, 15000, 150),
            "gemini-4-hydrogen" => (1500, 7500, 75),
            "gemini-4-argon" | "gemini-4-carbon" if date.timestamp() < 1_796_083_200 => {
                (2000, 10000, 100)
            }
            "gemini-4-argon" | "gemini-4-carbon" => (4000, 20000, 200),
            "gemini-3.8-flash" if date.timestamp() < 1_798_761_600 => (750, 3750, 75),
            "gemini-3.8-flash" => (1500, 7500, 150),
            _ => return None,
        };
        Some(ModelPricingRates {
            input: Decimal::new(input, 9),
            output: Decimal::new(output, 9),
            cache_read: Some(Decimal::new(read, 9)),
            ..Default::default()
        })
    }
}
