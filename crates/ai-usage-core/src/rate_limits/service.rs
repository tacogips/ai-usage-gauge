use super::{
    claude::ClaudeFetcher, codex::CodexFetcher, gemini, store::RateLimitObservationFileStore,
    transport::ReqwestRateLimitTransport,
};
use crate::{
    paths::AppPaths,
    types::{
        ProviderRateLimits, RateLimitErrorCode as Code, RateLimitObservation,
        RateLimitProvider as Provider, RateLimitStatus as Status, RateLimitWindowKind as Kind,
        RateLimitsResponse,
    },
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration,
};
use tokio::sync::Mutex;

pub trait RateLimitFetcher: Send + Sync {
    fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Pin<Box<dyn Future<Output = Result<RateLimitObservation, Code>> + Send + '_>>;
    fn fallback(&self) -> Option<RateLimitObservation> {
        None
    }
}
impl RateLimitFetcher for ClaudeFetcher {
    fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Pin<Box<dyn Future<Output = Result<RateLimitObservation, Code>> + Send + '_>> {
        Box::pin(ClaudeFetcher::fetch(self, now))
    }
}
impl RateLimitFetcher for CodexFetcher {
    fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Pin<Box<dyn Future<Output = Result<RateLimitObservation, Code>> + Send + '_>> {
        Box::pin(CodexFetcher::fetch(self, now))
    }
    fn fallback(&self) -> Option<RateLimitObservation> {
        CodexFetcher::fallback(self)
    }
}
struct GeminiFetcher;
impl RateLimitFetcher for GeminiFetcher {
    fn fetch(
        &self,
        _: DateTime<Utc>,
    ) -> Pin<Box<dyn Future<Output = Result<RateLimitObservation, Code>> + Send + '_>> {
        Box::pin(async {
            let path = gemini::discover().ok_or(Code::CliMissing)?;
            tokio::task::spawn_blocking(move || gemini::fetch(&path, Duration::from_secs(12)))
                .await
                .map_err(|_| Code::InvalidResponse)?
        })
    }
}
#[derive(Default)]
struct State {
    last_attempt: Option<DateTime<Utc>>,
    cached: Option<ProviderRateLimits>,
    last_good: Option<RateLimitObservation>,
    restored: bool,
    throttles: u32,
    retry_at: Option<DateTime<Utc>>,
}
struct Slot {
    provider: Provider,
    fetcher: Arc<dyn RateLimitFetcher>,
    interval: i64,
    state: Mutex<State>,
}
pub struct RateLimitUsageService {
    slots: Vec<Slot>,
    store: Option<RateLimitObservationFileStore>,
    store_lock: Mutex<()>,
}
impl RateLimitUsageService {
    pub fn new(
        claude: Arc<dyn RateLimitFetcher>,
        codex: Arc<dyn RateLimitFetcher>,
        gemini: Arc<dyn RateLimitFetcher>,
        store: Option<RateLimitObservationFileStore>,
    ) -> Self {
        Self {
            slots: vec![
                (Provider::Claude, claude, 300),
                (Provider::Codex, codex, 60),
                (Provider::Gemini, gemini, 60),
            ]
            .into_iter()
            .map(|(provider, fetcher, interval)| Slot {
                provider,
                fetcher,
                interval,
                state: Mutex::new(State::default()),
            })
            .collect(),
            store,
            store_lock: Mutex::new(()),
        }
    }
    pub fn production(paths: &AppPaths) -> Result<Self, Code> {
        let transport = Arc::new(ReqwestRateLimitTransport::new()?);
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Ok(Self::new(
            Arc::new(ClaudeFetcher::new(claude, transport.clone())),
            Arc::new(CodexFetcher::new(codex, transport)),
            Arc::new(GeminiFetcher),
            Some(RateLimitObservationFileStore::new(&paths.rate_limits_file)),
        ))
    }
    pub async fn current(&self) -> RateLimitsResponse {
        let now = Utc::now();
        let (a, b, c) = tokio::join!(self.entry(0, now), self.entry(1, now), self.entry(2, now));
        RateLimitsResponse {
            generated_at: now,
            scope: "local".into(),
            providers: vec![a, b, c],
        }
    }
    pub async fn entry(&self, index: usize, now: DateTime<Utc>) -> ProviderRateLimits {
        let slot = &self.slots[index];
        let mut state = slot.state.lock().await;
        let key = match slot.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
        };
        if !state.restored {
            state.restored = true;
            if let Some(store) = &self.store {
                state.last_good = store.load().ok().and_then(|mut m| m.remove(key));
            }
        }
        if let (Some(last), Some(cached)) = (state.last_attempt, &state.cached) {
            if now.signed_duration_since(last).num_seconds() < slot.interval
                || state.retry_at.is_some_and(|retry| now < retry)
            {
                return cached.clone();
            }
        }
        let outcome = slot.fetcher.fetch(now).await;
        state.last_attempt = Some(now);
        let result = match outcome {
            Ok(observation) => {
                state.throttles = 0;
                state.retry_at = None;
                state.last_good = Some(observation.clone());
                if let Some(store) = &self.store {
                    let _guard = self.store_lock.lock().await;
                    let mut all = store.load().unwrap_or_else(|_| BTreeMap::new());
                    all.insert(key.into(), observation.clone());
                    let _ = store.save(&all);
                }
                entry(slot.provider, Status::Ok, None, Some(observation))
            }
            Err(code) => {
                if code == Code::RateLimited {
                    state.throttles = state.throttles.saturating_add(1);
                    let multiplier = 1_i64 << state.throttles.saturating_sub(1).min(20);
                    let delay = (slot.interval * multiplier).min(1800);
                    state.retry_at = now.checked_add_signed(chrono::Duration::seconds(delay));
                }
                let candidate = if slot.provider == Provider::Claude
                    && matches!(code, Code::Unauthorized | Code::TokenExpired)
                {
                    None
                } else {
                    [state.last_good.clone(), slot.fetcher.fallback()]
                        .into_iter()
                        .flatten()
                        .max_by_key(|o| o.observed_at)
                        .and_then(|mut o| {
                            o.windows.retain(|w| w.resets_at.is_none_or(|r| r > now));
                            if o.windows.is_empty() {
                                None
                            } else {
                                Some(o)
                            }
                        })
                };
                let status = if candidate.is_some() {
                    Status::Stale
                } else if matches!(code, Code::Unauthorized | Code::TokenExpired) {
                    Status::Unauthorized
                } else {
                    Status::Unavailable
                };
                entry(slot.provider, status, Some(code), candidate)
            }
        };
        state.cached = Some(result.clone());
        result
    }
}
fn entry(
    provider: Provider,
    status: Status,
    code: Option<Code>,
    observation: Option<RateLimitObservation>,
) -> ProviderRateLimits {
    let mut result = ProviderRateLimits {
        provider,
        status,
        error_code: code,
        message: code.map(|c| {
            match c {
                Code::CliMissing => "Install the Google Antigravity CLI (agy)",
                Code::CredentialsMissing => "CLI credentials were not found",
                Code::CredentialsUnreadable => "CLI credentials could not be read",
                Code::TokenExpired | Code::Unauthorized => {
                    "Sign in again with the CLI to refresh credentials"
                }
                Code::RateLimited => "Usage API is throttling requests; retrying later",
                Code::HttpError => "Rate limit request failed",
                Code::NetworkError => "Rate limit network request failed",
                Code::Timeout => "Rate limit request timed out",
                Code::InvalidResponse => "Rate limit response was invalid",
            }
            .into()
        }),
        plan_type: observation.as_ref().and_then(|o| o.plan_type.clone()),
        fetched_at: observation.as_ref().map(|o| o.observed_at),
        stale: status == Status::Stale,
        five_hour: None,
        weekly: None,
        other_windows: vec![],
        credits: observation.as_ref().and_then(|o| o.credits.clone()),
    };
    if let Some(o) = observation {
        for w in o.windows {
            match w.kind {
                Kind::FiveHour if result.five_hour.is_none() => result.five_hour = Some(w),
                Kind::Weekly if result.weekly.is_none() => result.weekly = Some(w),
                _ => result.other_windows.push(w),
            }
        }
    }
    result
}
