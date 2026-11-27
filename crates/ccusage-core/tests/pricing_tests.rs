use ccusage_core::{
    pricing::{LiteLlmPricingCache, PricingEngine},
    types::{PricingHttpTransport, TimestampedUsageEvent},
};
use chrono::{TimeZone, Utc};
use rust_decimal::Decimal;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime},
};

struct Mock {
    calls: AtomicUsize,
    response: Result<String, String>,
}
impl PricingHttpTransport for Mock {
    fn get_text<'a>(
        &'a self,
        _: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { self.response.clone() })
    }
}
fn event(model: &str) -> TimestampedUsageEvent {
    TimestampedUsageEvent {
        identity: "i".into(),
        timestamp: Utc.timestamp_opt(1_796_083_199, 0).unwrap(),
        agent: "claude".into(),
        session_id: "s".into(),
        request_id: "r".into(),
        message_id: "m".into(),
        model: model.into(),
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_5m_tokens: 0,
        cache_creation_1h_tokens: 0,
        directory: None,
        effort: None,
    }
}
const JSON: &str = r#"{"anthropic/test-model":{"input_cost_per_token":"0.000001","output_cost_per_token":0.000002,"cache_creation_input_token_cost":"0.000003","cache_creation_input_token_cost_above_1hr":"0.000004","cache_read_input_token_cost":"0.0000005","input_cost_per_token_above_200k_tokens":"0.000002"}}"#;
fn mock(response: Result<String, String>) -> Mock {
    Mock {
        calls: AtomicUsize::new(0),
        response,
    }
}
#[tokio::test]
async fn refresh_and_offline_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache/pricing.json");
    let mut cache = LiteLlmPricingCache::new(&path);
    let transport = mock(Ok(JSON.into()));
    let now = SystemTime::now();
    let engine = cache.refresh(&transport, now).await;
    assert_eq!(
        engine.price_event(&event("test-model")).cost_usd,
        Decimal::new(18, 1)
    );
    cache
        .refresh(&transport, now + Duration::from_secs(60))
        .await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    for response in [Err("offline or HTTP 500".into()), Ok("not json".into())] {
        let mut stale = LiteLlmPricingCache::new(&path);
        let bad = mock(response);
        let e = stale.refresh(&bad, now + Duration::from_secs(86401)).await;
        assert_eq!(
            e.price_event(&event("test-model")).cost_usd,
            Decimal::new(18, 1)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), JSON);
    }
    let mut missing = LiteLlmPricingCache::new(dir.path().join("missing"));
    assert_eq!(
        missing
            .refresh(&mock(Err("offline".into())), now)
            .await
            .price_event(&event("claude-sonnet-4"))
            .cost_usd,
        Decimal::from(3)
    );
}
#[tokio::test]
async fn stale_cache_is_refreshed_and_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    std::fs::write(&path, JSON).unwrap();
    let replacement =
        r#"{"test-model":{"input_cost_per_token":"0.000004","output_cost_per_token":"0.000002"}}"#;
    let transport = mock(Ok(replacement.into()));
    let mut cache = LiteLlmPricingCache::new(&path);
    let now = SystemTime::now() + Duration::from_secs(86401);
    let engine = cache.refresh(&transport, now).await;
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
    assert_eq!(
        engine.price_event(&event("test-model")).cost_usd,
        Decimal::from(4)
    );
}
#[tokio::test]
async fn resolution_and_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = LiteLlmPricingCache::new(dir.path().join("p"));
    let engine = cache
        .refresh(&mock(Ok(JSON.into())), SystemTime::now())
        .await;
    let mut e = event("TEST-MODEL-20250514");
    e.input_tokens = 250_000;
    e.cache_creation_tokens = 100;
    e.cache_creation_5m_tokens = 20;
    e.cache_creation_1h_tokens = 30;
    e.cache_read_tokens = 40;
    assert_eq!(engine.price_event(&e).cost_usd, Decimal::new(300350, 6));
    assert_eq!(
        PricingEngine::default()
            .price_event(&event("anthropic/claude-sonnet-4-2025-08-07"))
            .cost_usd,
        Decimal::from(3)
    );
    let unknown = event("unknown");
    let priced = engine.price_event(&unknown);
    assert_eq!(priced.cost_usd, Decimal::ZERO);
    assert_eq!(priced.event.input_tokens, unknown.input_tokens);
    let mut fallback = event("claude-sonnet-4");
    fallback.input_tokens = 0;
    fallback.cache_creation_tokens = 100;
    fallback.cache_creation_1h_tokens = 30;
    fallback.cache_read_tokens = 40;
    assert_eq!(
        PricingEngine::default().price_event(&fallback).cost_usd,
        Decimal::new(4545, 7)
    );
}
#[test]
fn date_effective_schedule() {
    let engine = PricingEngine::default();
    let mut e = event("gemini-4-argon");
    e.agent = "agy".into();
    assert_eq!(engine.price_event(&e).cost_usd, Decimal::from(2));
    e.timestamp = Utc.timestamp_opt(1_796_083_200, 0).unwrap();
    assert_eq!(engine.price_event(&e).cost_usd, Decimal::from(4));
    e.model = "gemini-3.8-flash".into();
    e.timestamp = Utc.timestamp_opt(1_798_761_599, 0).unwrap();
    assert_eq!(engine.price_event(&e).cost_usd, Decimal::new(75, 2));
    e.timestamp = Utc.timestamp_opt(1_798_761_600, 0).unwrap();
    assert_eq!(engine.price_event(&e).cost_usd, Decimal::new(15, 1));
}
