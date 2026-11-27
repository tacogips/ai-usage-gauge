use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ApiClientError {
    #[error("dashboard API unreachable at {url}: {message}")]
    Unreachable { url: String, message: String },
    #[error("API rejected request (HTTP {status}): {body}")]
    ClientError { status: u16, body: String },
    #[error("API server error (HTTP {status}): {body}")]
    ServerError { status: u16, body: String },
}

impl ApiClientError {
    /// Exit code mapping matching `design-docs/specs/client-commands.md`:
    /// 3 = unreachable, 4 = 4xx rejection, 5 = 5xx server error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Unreachable { .. } => 3,
            Self::ClientError { .. } => 4,
            Self::ServerError { .. } => 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DashboardAPIClient {
    base_url: String,
    http: reqwest::Client,
}

impl DashboardAPIClient {
    pub fn new(port: u16) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        Ok(Self {
            base_url: format!("http://127.0.0.1:{port}"),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn request_raw(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<Vec<u8>>,
        is_mutation: bool,
    ) -> Result<(u16, String), ApiClientError> {
        let url = format!("{}{}", self.base_url, path_and_query);
        let parsed_method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        let mut req = self.http.request(parsed_method, &url);
        if is_mutation {
            req = req.header("X-CCUsage-Gauge-Mutation", "1");
        }
        if let Some(bytes) = body {
            req = req.header("Content-Type", "application/json").body(bytes);
        }
        let resp = req.send().await.map_err(|e| ApiClientError::Unreachable {
            url: url.clone(),
            message: e.to_string(),
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok((status, text))
        } else if (400..500).contains(&status) {
            Err(ApiClientError::ClientError { status, body: text })
        } else {
            Err(ApiClientError::ServerError { status, body: text })
        }
    }

    pub async fn get_json(&self, path_and_query: &str) -> Result<Value, ApiClientError> {
        let is_mutation = path_and_query.starts_with("/api/refresh");
        let (_, text) = self
            .request_raw("GET", path_and_query, None, is_mutation)
            .await?;
        serde_json::from_str(&text).map_err(|e| ApiClientError::ServerError {
            status: 500,
            body: format!("invalid JSON from server: {e}"),
        })
    }

    pub async fn mutate_json(
        &self,
        method: &str,
        path_and_query: &str,
        payload: Option<&Value>,
    ) -> Result<(u16, Value), ApiClientError> {
        let body = payload.map(|v| serde_json::to_vec(v).unwrap_or_default());
        let (status, text) = self.request_raw(method, path_and_query, body, true).await?;
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
    }
}
