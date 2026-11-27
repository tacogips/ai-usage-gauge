use crate::types::{
    PieChartId as Id, ProviderRateLimits, RateLimitCreditUnit, RateLimitCredits, RateLimitStatus,
    RateLimitWindow, RateLimitsResponse,
};
use chrono::{DateTime, Utc};

pub fn title(id: Id) -> &'static str {
    match id {
        Id::Budget => "Budget",
        Id::Claude5h => "Claude Code 5h",
        Id::ClaudeWeekly => "Claude Code weekly",
        Id::ClaudeCredit => "Claude Code credit",
        Id::Codex5h => "Codex 5h",
        Id::CodexWeekly => "Codex weekly",
        Id::CodexCredit => "Codex credit",
        Id::Gemini5h => "Gemini 5h",
        Id::GeminiWeekly => "Gemini weekly",
    }
}
fn entry(
    id: Id,
    response: Option<&RateLimitsResponse>,
) -> (Option<&ProviderRateLimits>, Option<&RateLimitWindow>) {
    let provider = response.and_then(|r| {
        r.providers
            .iter()
            .find(|p| Some(p.provider) == id.provider())
    });
    let window = provider.and_then(|p| match id {
        Id::Claude5h | Id::Codex5h | Id::Gemini5h => p.five_hour.as_ref(),
        Id::ClaudeWeekly | Id::CodexWeekly | Id::GeminiWeekly => p.weekly.as_ref(),
        _ => None,
    });
    (provider, window)
}
pub fn ratio_label(id: Id, response: Option<&RateLimitsResponse>) -> String {
    if id == Id::Budget {
        return String::new();
    }
    let (provider, window) = entry(id, response);
    let value = if id.is_credit() {
        provider
            .and_then(|p| p.credits.as_ref())
            .and_then(|c| c.used_percent)
    } else {
        window.map(|w| w.used_percent)
    };
    value
        .map(|v| format!("{:.0}%", v.clamp(0.0, 100.0)))
        .unwrap_or_else(|| "--".into())
}
pub fn reset_text(reset: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(reset) = reset else {
        return "reset time unknown".into();
    };
    let seconds = reset.signed_duration_since(now).num_seconds();
    if seconds <= 0 {
        return "reset due".into();
    }
    if seconds < 60 {
        return "reset <1m".into();
    }
    let minutes = seconds / 60;
    if minutes >= 1440 {
        format!("reset {}d {}h", minutes / 1440, minutes % 1440 / 60)
    } else {
        format!("reset {}h {}m", minutes / 60, minutes % 60)
    }
}
pub fn credit_summary(c: &RateLimitCredits) -> String {
    if c.unlimited {
        return "unlimited".into();
    }
    if !c.enabled {
        return if c.unit == RateLimitCreditUnit::Usd {
            "not enabled"
        } else {
            "no credits"
        }
        .into();
    }
    match c.unit {
        RateLimitCreditUnit::Usd => match (c.used, c.limit, c.used_percent) {
            (Some(u), Some(l), p) => format!(
                "{} / {} used{}",
                usd(u),
                usd(l),
                p.map(|p| format!(" ({p:.0}%)")).unwrap_or_default()
            ),
            (Some(u), _, _) => format!("{} used", usd(u)),
            (_, _, Some(p)) => format!("{p:.0}% used"),
            _ => "enabled".into(),
        },
        RateLimitCreditUnit::Credits => c
            .balance
            .map(|v| format!("{} credits left", number(v)))
            .unwrap_or_else(|| "available".into()),
    }
}
fn number(value: f64) -> String {
    let formatted = if value.round() == value {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    };
    let (integer, fraction) = formatted.split_once('.').unwrap_or((&formatted, ""));
    let (sign, digits) = integer
        .strip_prefix('-')
        .map_or(("", integer), |s| ("-", s));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{fraction}")
    }
}
fn usd(value: f64) -> String {
    if value.round() == value {
        format!("${}", number(value))
    } else {
        format!("${value:.2}")
    }
}
pub fn compact_menu_line(
    id: Id,
    response: Option<&RateLimitsResponse>,
    now: DateTime<Utc>,
) -> String {
    let (provider, window) = entry(id, response);
    let name = title(id);
    let Some(provider) = provider else {
        return format!("{name} · Loading");
    };
    let stale = if provider.status == RateLimitStatus::Stale {
        " (stale)"
    } else {
        ""
    };
    if id.is_credit() {
        if let Some(c) = &provider.credits {
            return format!("{name} · {}{stale}", credit_summary(c));
        }
    }
    if let Some(w) = window {
        return format!(
            "{name} · {:.0}% left · {}{stale}",
            w.remaining_percent,
            reset_text(w.resets_at, now)
        );
    }
    format!(
        "{name} · {}",
        if provider.status == RateLimitStatus::Unauthorized {
            "Sign in"
        } else {
            "Unavailable"
        }
    )
}
pub fn credit_lines(response: Option<&RateLimitsResponse>) -> Vec<String> {
    [Id::ClaudeCredit, Id::CodexCredit]
        .into_iter()
        .filter_map(|id| {
            let (p, _) = entry(id, response);
            let p = p?;
            let c = p.credits.as_ref()?;
            if !c.enabled && !c.unlimited {
                return None;
            }
            Some(format!(
                "{}: {}{}",
                title(id),
                credit_summary(c),
                if p.stale { " (stale)" } else { "" }
            ))
        })
        .collect()
}
