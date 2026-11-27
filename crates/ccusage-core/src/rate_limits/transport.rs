use std::fmt;
use std::time::Duration;

use crate::types::RateLimitErrorCode;

#[derive(Clone)]
pub struct RedactedSecret(String);

impl RedactedSecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl fmt::Display for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

pub trait RateLimitHttpTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'static str,
        headers: Vec<(&'static str, RedactedSecret)>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, RateLimitErrorCode>>
                + Send
                + 'a,
        >,
    >;
}

pub struct ReqwestRateLimitTransport(reqwest::Client);
impl ReqwestRateLimitTransport {
    pub fn new() -> Result<Self, RateLimitErrorCode> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map(Self)
            .map_err(|_| RateLimitErrorCode::NetworkError)
    }
}
impl RateLimitHttpTransport for ReqwestRateLimitTransport {
    fn get<'a>(
        &'a self,
        url: &'static str,
        headers: Vec<(&'static str, RedactedSecret)>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, RateLimitErrorCode>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut request = self.0.get(url);
            for (name, value) in headers {
                request = request.header(name, value.expose());
            }
            let response = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    RateLimitErrorCode::Timeout
                } else {
                    RateLimitErrorCode::NetworkError
                }
            })?;
            match response.status().as_u16() {
                200 => response
                    .json()
                    .await
                    .map_err(|_| RateLimitErrorCode::InvalidResponse),
                401 | 403 => Err(RateLimitErrorCode::Unauthorized),
                429 => Err(RateLimitErrorCode::RateLimited),
                _ => Err(RateLimitErrorCode::HttpError),
            }
        })
    }
}
