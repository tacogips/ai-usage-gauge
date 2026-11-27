use super::{
    date,
    transport::{RateLimitHttpTransport, RedactedSecret},
    window,
};
use crate::types::{
    RateLimitCreditUnit, RateLimitCredits, RateLimitErrorCode, RateLimitObservation,
    RateLimitWindow,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(serde::Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
}
#[derive(serde::Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

pub struct CodexFetcher {
    pub home: PathBuf,
    pub transport: Arc<dyn RateLimitHttpTransport>,
}
impl CodexFetcher {
    pub fn new(home: PathBuf, transport: Arc<dyn RateLimitHttpTransport>) -> Self {
        Self { home, transport }
    }
    pub async fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Result<RateLimitObservation, RateLimitErrorCode> {
        let bytes = std::fs::read(self.home.join("auth.json")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RateLimitErrorCode::CredentialsMissing
            } else {
                RateLimitErrorCode::CredentialsUnreadable
            }
        })?;
        let auth: AuthFile = serde_json::from_slice(&bytes)
            .map_err(|_| RateLimitErrorCode::CredentialsUnreadable)?;
        let tokens = auth.tokens.ok_or(RateLimitErrorCode::CredentialsMissing)?;
        let token = tokens
            .access_token
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or(RateLimitErrorCode::CredentialsMissing)?;
        let account = tokens
            .account_id
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or(RateLimitErrorCode::CredentialsMissing)?;
        let body = self
            .transport
            .get(
                "https://chatgpt.com/backend-api/wham/usage",
                vec![
                    (
                        "authorization",
                        RedactedSecret::new(format!("Bearer {token}")),
                    ),
                    ("chatgpt-account-id", RedactedSecret::new(account.into())),
                ],
            )
            .await?;
        parse_live(&body, now)
    }
    pub fn fallback(&self) -> Option<RateLimitObservation> {
        fallback(&self.home.join("sessions"))
    }
}
fn epoch(v: &Value) -> Option<DateTime<Utc>> {
    v.as_f64()
        .filter(|s| s.is_finite() && s.abs() < i64::MAX as f64 / 1000.0)
        .and_then(|s| DateTime::from_timestamp_millis((s * 1000.0) as i64))
}
fn reset(v: &Value, absolute: &str, relative: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    epoch(&v[absolute]).or_else(|| {
        v[relative]
            .as_f64()
            .filter(|s| s.is_finite() && s.abs() < i64::MAX as f64 / 1000.0)
            .and_then(|s| now.checked_add_signed(Duration::try_milliseconds((s * 1000.0) as i64)?))
    })
}
fn credits(value: &Value) -> Option<RateLimitCredits> {
    let enabled = value["has_credits"].as_bool();
    if enabled.is_none() && value["unlimited"].as_bool().is_none() {
        return None;
    }
    let unlimited = value["unlimited"].as_bool().unwrap_or(false);
    let balance = value["balance"]
        .as_f64()
        .or_else(|| value["balance"].as_str().and_then(|s| s.parse().ok()));
    Some(RateLimitCredits {
        unit: RateLimitCreditUnit::Credits,
        enabled: enabled.unwrap_or(unlimited),
        unlimited,
        used: None,
        limit: None,
        balance,
        used_percent: None,
    })
}
fn live_window(
    v: &Value,
    now: DateTime<Utc>,
) -> Result<Option<RateLimitWindow>, RateLimitErrorCode> {
    if v.is_null() {
        return Ok(None);
    }
    if !v.is_object() {
        return Err(RateLimitErrorCode::InvalidResponse);
    }
    let Some(used) = v.get("used_percent").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let used = used.as_f64().ok_or(RateLimitErrorCode::InvalidResponse)?;
    let Some(seconds) = v.get("limit_window_seconds").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let seconds = seconds
        .as_u64()
        .ok_or(RateLimitErrorCode::InvalidResponse)?;
    window(
        seconds,
        used,
        reset(v, "reset_at", "reset_after_seconds", now),
    )
    .map(Some)
}
pub fn parse_live(
    v: &Value,
    now: DateTime<Utc>,
) -> Result<RateLimitObservation, RateLimitErrorCode> {
    if !v.is_object() {
        return Err(RateLimitErrorCode::InvalidResponse);
    }
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        if let Some(w) = live_window(&v["rate_limit"][key], now)? {
            windows.push(w);
        }
    }
    Ok(RateLimitObservation {
        plan_type: v["plan_type"].as_str().map(str::to_owned),
        observed_at: now,
        windows,
        credits: credits(&v["credits"]),
    })
}
fn files(root: &Path, output: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, output);
            } else if path.extension().is_some_and(|s| s == "jsonl") {
                output.push(path);
            }
        }
    }
}
pub fn fallback(root: &Path) -> Option<RateLimitObservation> {
    let mut paths = Vec::new();
    files(root, &mut paths);
    paths.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    for path in paths.into_iter().take(10) {
        if let Some(observation) = reverse_lines(&path, |line| {
            if !line
                .windows(b"token_count".len())
                .any(|w| w == b"token_count")
            {
                return None;
            }
            let v = serde_json::from_slice::<Value>(line).ok()?;
            if v["type"] != "event_msg"
                || v["payload"]["type"] != "token_count"
                || !v["payload"]["rate_limits"].is_object()
            {
                return None;
            }
            let now = date(v["timestamp"].as_str())?;
            let limits = &v["payload"]["rate_limits"];
            let mut windows = Vec::new();
            for key in ["primary", "secondary"] {
                let w = &limits[key];
                if let (Some(minutes), Some(used)) =
                    (w["window_minutes"].as_u64(), w["used_percent"].as_f64())
                {
                    if let Some(seconds) = minutes.checked_mul(60) {
                        if let Ok(w) = window(
                            seconds,
                            used,
                            reset(w, "resets_at", "resets_in_seconds", now),
                        ) {
                            windows.push(w);
                        }
                    }
                }
            }
            Some(RateLimitObservation {
                plan_type: limits["plan_type"].as_str().map(str::to_owned),
                observed_at: now,
                windows,
                credits: credits(&limits["credits"]),
            })
        }) {
            return Some(observation);
        }
    }
    None
}

fn reverse_lines<T>(path: &Path, mut visit: impl FnMut(&[u8]) -> Option<T>) -> Option<T> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut position = file.metadata().ok()?.len();
    let mut tail = Vec::new();
    while position > 0 {
        let count = position.min(8192) as usize;
        position -= count as u64;
        file.seek(SeekFrom::Start(position)).ok()?;
        let mut chunk = vec![0; count];
        file.read_exact(&mut chunk).ok()?;
        chunk.extend_from_slice(&tail);
        let mut end = chunk.len();
        for i in (0..chunk.len()).rev() {
            if chunk[i] == b'\n' {
                if let Some(value) = visit(&chunk[i + 1..end]) {
                    return Some(value);
                }
                end = i;
            }
        }
        tail = chunk[..end].to_vec();
    }
    visit(&tail)
}
