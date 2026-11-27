pub mod claude;
pub mod codex;
pub mod formatting;
pub mod gemini;
pub mod service;
pub mod store;
pub mod transport;

use crate::types::{RateLimitErrorCode, RateLimitWindow, RateLimitWindowKind};
use chrono::{DateTime, Utc};

fn window(
    seconds: u64,
    used: f64,
    reset: Option<DateTime<Utc>>,
) -> Result<RateLimitWindow, RateLimitErrorCode> {
    if seconds == 0 || !used.is_finite() || used < 0.0 {
        return Err(RateLimitErrorCode::InvalidResponse);
    }
    Ok(RateLimitWindow {
        kind: RateLimitWindowKind::classify(seconds),
        window_seconds: seconds,
        used_percent: used,
        remaining_percent: (100.0 - used).clamp(0.0, 100.0),
        resets_at: reset,
    })
}
fn date(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}
