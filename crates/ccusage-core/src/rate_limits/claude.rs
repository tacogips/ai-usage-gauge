use super::{
    date,
    transport::{RateLimitHttpTransport, RedactedSecret},
    window,
};
use crate::types::{
    RateLimitCreditUnit, RateLimitCredits, RateLimitErrorCode, RateLimitObservation,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialFile {
    claude_ai_oauth: OAuthCredential,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthCredential {
    access_token: String,
    expires_at: Option<f64>,
    subscription_type: Option<String>,
}

pub type CredentialReader = Arc<dyn Fn(&Path) -> Result<Vec<u8>, RateLimitErrorCode> + Send + Sync>;

pub struct ClaudeFetcher {
    pub home: PathBuf,
    pub transport: Arc<dyn RateLimitHttpTransport>,
    credential_reader: CredentialReader,
}
impl ClaudeFetcher {
    pub fn new(home: PathBuf, transport: Arc<dyn RateLimitHttpTransport>) -> Self {
        Self {
            home,
            transport,
            credential_reader: Arc::new(credentials),
        }
    }
    pub fn with_credential_reader(mut self, reader: CredentialReader) -> Self {
        self.credential_reader = reader;
        self
    }
    pub async fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Result<RateLimitObservation, RateLimitErrorCode> {
        let reader = self.credential_reader.clone();
        let home = self.home.clone();
        let data = tokio::task::spawn_blocking(move || reader(&home))
            .await
            .map_err(|_| RateLimitErrorCode::CredentialsUnreadable)??;
        let json: CredentialFile =
            serde_json::from_slice(&data).map_err(|_| RateLimitErrorCode::CredentialsUnreadable)?;
        let oauth = json.claude_ai_oauth;
        let token = oauth.access_token;
        if token.is_empty() {
            return Err(RateLimitErrorCode::CredentialsUnreadable);
        }
        if oauth
            .expires_at
            .is_some_and(|expiry| expiry <= now.timestamp_millis() as f64)
        {
            return Err(RateLimitErrorCode::TokenExpired);
        }
        let plan = oauth.subscription_type;
        let body = self
            .transport
            .get(
                "https://api.anthropic.com/api/oauth/usage",
                vec![
                    (
                        "authorization",
                        RedactedSecret::new(format!("Bearer {token}")),
                    ),
                    (
                        "anthropic-beta",
                        RedactedSecret::new("oauth-2025-04-20".into()),
                    ),
                    (
                        "content-type",
                        RedactedSecret::new("application/json".into()),
                    ),
                ],
            )
            .await?;
        parse(&body, plan, now)
    }
}
fn credentials(home: &Path) -> Result<Vec<u8>, RateLimitErrorCode> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        if let Ok(mut child) = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            let start = Instant::now();
            loop {
                if let Ok(Some(status)) = child.try_wait() {
                    if status.success() {
                        use std::io::Read;
                        let mut output = Vec::new();
                        if let Some(mut stdout) = child.stdout.take() {
                            let _ = stdout.read_to_end(&mut output);
                        }
                        if !output.is_empty() {
                            return Ok(output);
                        }
                    }
                    break;
                }
                if start.elapsed() >= Duration::from_secs(10) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    std::fs::read(home.join(".credentials.json")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RateLimitErrorCode::CredentialsMissing
        } else {
            RateLimitErrorCode::CredentialsUnreadable
        }
    })
}
pub fn parse(
    body: &Value,
    plan_type: Option<String>,
    now: DateTime<Utc>,
) -> Result<RateLimitObservation, RateLimitErrorCode> {
    if !body.is_object() {
        return Err(RateLimitErrorCode::InvalidResponse);
    }
    let mut windows = Vec::new();
    for (key, seconds) in [("five_hour", 18_000), ("seven_day", 604_800)] {
        if let Some(value) = body.get(key).filter(|v| !v.is_null()) {
            if !value.is_object() {
                return Err(RateLimitErrorCode::InvalidResponse);
            }
            let Some(utilization) = value.get("utilization").filter(|v| !v.is_null()) else {
                continue;
            };
            let used = utilization
                .as_f64()
                .ok_or(RateLimitErrorCode::InvalidResponse)?;
            windows.push(window(seconds, used, date(value["resets_at"].as_str()))?);
        }
    }
    let extra = &body["extra_usage"];
    let credits = extra["is_enabled"].as_bool().map(|enabled| {
        let used = extra["used_credits"].as_f64().map(|n| n / 100.0);
        let limit = extra["monthly_limit"].as_f64().map(|n| n / 100.0);
        let percent = extra["utilization"].as_f64().or_else(|| {
            used.zip(limit)
                .filter(|(_, l)| *l > 0.0)
                .map(|(u, l)| u / l * 100.0)
        });
        RateLimitCredits {
            unit: RateLimitCreditUnit::Usd,
            enabled,
            unlimited: false,
            used,
            limit,
            balance: None,
            used_percent: percent,
        }
    });
    Ok(RateLimitObservation {
        plan_type,
        observed_at: now,
        windows,
        credits,
    })
}
