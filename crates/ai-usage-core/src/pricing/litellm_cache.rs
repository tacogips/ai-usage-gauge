use super::{ModelPricingRates, PricingEngine};
use crate::types::PricingHttpTransport;
use rust_decimal::Decimal;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime},
};

const URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const DAY: Duration = Duration::from_secs(86400);

pub struct ReqwestPricingTransport {
    client: reqwest::Client,
}
impl Default for ReqwestPricingTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}
impl PricingHttpTransport for ReqwestPricingTransport {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            self.client
                .get(url)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .text()
                .await
                .map_err(|e| e.to_string())
        })
    }
}

pub struct LiteLlmPricingCache {
    path: PathBuf,
    last_attempt: Option<SystemTime>,
}
impl LiteLlmPricingCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_attempt: None,
        }
    }
    pub fn production() -> Self {
        Self::new(crate::paths::AppPaths::production().litellm_pricing_cache_file)
    }
    pub fn engine(&self) -> PricingEngine {
        PricingEngine::new(read(&self.path).unwrap_or_default())
    }
    pub async fn refresh(
        &mut self,
        transport: &dyn PricingHttpTransport,
        now: SystemTime,
    ) -> PricingEngine {
        let disk = read(&self.path);
        let fresh = disk.is_some()
            && std::fs::metadata(&self.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age < DAY);
        let attempted = self
            .last_attempt
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age < DAY);
        if fresh || attempted {
            return PricingEngine::new(disk.unwrap_or_default());
        }
        self.last_attempt = Some(now);
        if let Ok(Ok(body)) =
            tokio::time::timeout(Duration::from_secs(10), transport.get_text(URL)).await
        {
            if let Some(table) = parse(&body) {
                if write_atomic(&self.path, body.as_bytes()).is_ok() {
                    return PricingEngine::new(table);
                }
            }
        }
        PricingEngine::new(disk.unwrap_or_default())
    }
}
fn decimal(v: Option<&Value>) -> Option<Decimal> {
    let s = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.contains(['e', 'E']) {
        Decimal::from_scientific(&s)
    } else {
        Decimal::from_str(&s)
    }
    .ok()
    .filter(|n| *n >= Decimal::ZERO)
}
fn parse(body: &str) -> Option<HashMap<String, ModelPricingRates>> {
    let json: Value = serde_json::from_str(body).ok()?;
    let mut table = HashMap::new();
    for (name, entry) in json.as_object()? {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let (Some(input), Some(output)) = (
            decimal(obj.get("input_cost_per_token")),
            decimal(obj.get("output_cost_per_token")),
        ) else {
            continue;
        };
        table.insert(
            name.to_lowercase(),
            ModelPricingRates {
                input,
                output,
                cache_read: decimal(obj.get("cache_read_input_token_cost")),
                cache_creation_5m: decimal(obj.get("cache_creation_input_token_cost")),
                cache_creation_1h: decimal(obj.get("cache_creation_input_token_cost_above_1hr")),
                input_above_200k: decimal(obj.get("input_cost_per_token_above_200k_tokens")),
                output_above_200k: decimal(obj.get("output_cost_per_token_above_200k_tokens")),
                cache_read_above_200k: decimal(
                    obj.get("cache_read_input_token_cost_above_200k_tokens"),
                ),
                cache_creation_above_200k: decimal(
                    obj.get("cache_creation_input_token_cost_above_200k_tokens"),
                ),
            },
        );
    }
    if table.is_empty() {
        None
    } else {
        Some(table)
    }
}
fn read(path: &Path) -> Option<HashMap<String, ModelPricingRates>> {
    parse(&std::fs::read_to_string(path).ok()?)
}
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(data)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
