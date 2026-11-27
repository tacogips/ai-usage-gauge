use chrono::Utc;
use chrono_tz::Tz;
use rust_decimal::Decimal;

use ccusage_core::state::{ResetWindowCalculator, StateStore};
use ccusage_core::types::{AppState, ResetCycle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogValidationError {
    NegativeBudget,
    ZeroCustomHours,
    ZeroRefreshInterval,
    Persist(String),
}

impl std::fmt::Display for DialogValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NegativeBudget => f.write_str("budget must be non-negative"),
            Self::ZeroCustomHours => f.write_str("custom hours must be positive"),
            Self::ZeroRefreshInterval => f.write_str("refresh interval must be positive"),
            Self::Persist(msg) => write!(f, "failed to persist state: {msg}"),
        }
    }
}

impl std::error::Error for DialogValidationError {}

pub fn validate_budget_usd(
    value: Option<Decimal>,
) -> Result<Option<Decimal>, DialogValidationError> {
    if let Some(v) = value {
        if v.is_sign_negative() {
            return Err(DialogValidationError::NegativeBudget);
        }
    }
    Ok(value)
}

pub fn validate_custom_hours(hours: u32) -> Result<u32, DialogValidationError> {
    if hours == 0 {
        return Err(DialogValidationError::ZeroCustomHours);
    }
    Ok(hours)
}

pub fn validate_refresh_interval_seconds(
    seconds: Option<u32>,
) -> Result<Option<u32>, DialogValidationError> {
    if seconds == Some(0) {
        return Err(DialogValidationError::ZeroRefreshInterval);
    }
    Ok(seconds)
}

pub fn apply_budget_update(
    store: &StateStore,
    state: &AppState,
    budget_usd: Option<Decimal>,
) -> Result<AppState, DialogValidationError> {
    let valid = validate_budget_usd(budget_usd)?;
    let mut next = state.clone();
    next.budget_usd = valid;
    store
        .save(&next)
        .map_err(|e| DialogValidationError::Persist(e.to_string()))?;
    Ok(next)
}

pub fn apply_custom_hours_update(
    store: &StateStore,
    state: &AppState,
    hours: u32,
    tz: Tz,
) -> Result<AppState, DialogValidationError> {
    let valid = validate_custom_hours(hours)?;
    let calc = ResetWindowCalculator::new(tz);
    let next = calc
        .changing(state, ResetCycle::CustomHours(valid), Utc::now())
        .map_err(|e| DialogValidationError::Persist(e.to_string()))?;
    store
        .save(&next)
        .map_err(|e| DialogValidationError::Persist(e.to_string()))?;
    Ok(next)
}

pub fn apply_refresh_interval_update(
    store: &StateStore,
    state: &AppState,
    seconds: Option<u32>,
) -> Result<AppState, DialogValidationError> {
    let valid = validate_refresh_interval_seconds(seconds)?;
    let mut next = state.clone();
    next.refresh_interval_seconds = valid;
    store
        .save(&next)
        .map_err(|e| DialogValidationError::Persist(e.to_string()))?;
    Ok(next)
}

pub fn render_dialog_html(dialog: &str) -> (&'static str, String) {
    let title = match dialog {
        "budget" => "Set Budget",
        "custom-hours" => "Custom Aggregation Hours",
        "refresh-interval" => "Refresh Interval",
        "display-settings" => "Menu Bar Display",
        _ => "Settings",
    };
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{title}</title>
<style>body{{font-family:-apple-system,sans-serif;padding:16px;background:#1e1e1e;color:#eee}}input,button{{padding:6px 10px;margin-top:8px}}</style>
</head><body><h3>{title}</h3><p>Use the native menu controls or CLI to update {dialog}.</p></body></html>"#
    );
    ("text/html; charset=utf-8", html)
}
