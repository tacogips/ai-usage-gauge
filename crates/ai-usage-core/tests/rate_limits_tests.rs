use ai_usage_core::rate_limits::store::RateLimitObservationFileStore;
use ai_usage_core::rate_limits::transport::RedactedSecret;
use ai_usage_core::types::RateLimitObservation;
use std::collections::BTreeMap;

#[test]
fn secret_formats_are_redacted() {
    let secret = RedactedSecret::new("test-secret".into());
    assert_eq!(format!("{secret}"), "[REDACTED]");
    assert_eq!(format!("{secret:?}"), "[REDACTED]");
}

#[test]
fn observation_store_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state").join("rate-limits.json");
    let store = RateLimitObservationFileStore::new(&path);
    let observation = RateLimitObservation {
        plan_type: Some("test".into()),
        observed_at: chrono::Utc::now(),
        windows: vec![],
        credits: None,
    };
    let values = ["claude", "codex", "gemini"]
        .into_iter()
        .map(|name| (name.to_string(), observation.clone()))
        .collect::<BTreeMap<_, _>>();
    store.save(&values).unwrap();
    assert_eq!(store.load().unwrap(), values);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

macro_rules! async_fixtures { ($($item:item)*) => { $($item)* }; }

#[rustfmt::skip]
async_fixtures! {
#[test]
fn null_window_fields_preserve_valid_siblings_and_reject_invalid_values() {
    use ai_usage_core::rate_limits::{claude, codex};
    let now = chrono::Utc::now();
    let result = claude::parse(&serde_json::json!({"five_hour":{"utilization":null},"seven_day":{"utilization":53.0},"extra_usage":{"is_enabled":true}}), None, now).unwrap();
    assert_eq!(result.windows.len(), 1);
    assert_eq!(result.windows[0].remaining_percent, 47.0);
    assert!(result.credits.unwrap().enabled);
    for input in [serde_json::json!({"five_hour":{"utilization":null}}), serde_json::json!({"five_hour":{}})] {
        assert!(claude::parse(&input, None, now).unwrap().windows.is_empty());
    }
    for value in [serde_json::json!(-5), serde_json::json!("bad")] {
        assert_eq!(claude::parse(&serde_json::json!({"five_hour":{"utilization":value}}), None, now).unwrap_err(), RateLimitErrorCode::InvalidResponse);
    }
    assert_eq!(claude::parse(&serde_json::json!({"five_hour":"bad"}), None, now).unwrap_err(), RateLimitErrorCode::InvalidResponse);
    for primary in [serde_json::json!({"used_percent":10,"limit_window_seconds":null}), serde_json::json!({"used_percent":null,"limit_window_seconds":18000}), serde_json::json!({"used_percent":10}), serde_json::json!({"limit_window_seconds":18000})] {
        let input = serde_json::json!({"rate_limit":{"primary_window":primary,"secondary_window":{"used_percent":20,"limit_window_seconds":604800}}});
        let result = codex::parse_live(&input, now).unwrap();
        assert_eq!(result.windows.len(), 1);
        assert_eq!(result.windows[0].window_seconds, 604800);
    }
    for primary in [serde_json::json!("bad"), serde_json::json!({"used_percent":"bad","limit_window_seconds":18000}), serde_json::json!({"used_percent":-1,"limit_window_seconds":18000}), serde_json::json!({"used_percent":10,"limit_window_seconds":0}), serde_json::json!({"used_percent":10,"limit_window_seconds":-1}), serde_json::json!({"used_percent":10,"limit_window_seconds":"bad"})] {
        assert_eq!(codex::parse_live(&serde_json::json!({"rate_limit":{"primary_window":primary}}), now).unwrap_err(), RateLimitErrorCode::InvalidResponse);
    }
}
use ai_usage_core::rate_limits::{
    claude::ClaudeFetcher,
    codex::CodexFetcher,
    formatting, gemini,
    service::{RateLimitFetcher, RateLimitUsageService},
    transport::RateLimitHttpTransport,
};
use ai_usage_core::types::{
    PieChartId, RateLimitErrorCode, RateLimitStatus, RateLimitWindow, RateLimitWindowKind,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct MockTransport(Value);
impl RateLimitHttpTransport for MockTransport {
    fn get<'a>(
        &'a self,
        _: &'static str,
        _: Vec<(&'static str, RedactedSecret)>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, RateLimitErrorCode>> + Send + 'a>> {
        Box::pin(std::future::ready(Ok(self.0.clone())))
    }
}
#[tokio::test]
async fn claude_fetch_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    std::fs::write(&path, json!({"claudeAiOauth":{"accessToken":"test-token","expiresAt": 4102444800000_i64,"subscriptionType":"max"}}).to_string()).unwrap();
    let response = json!({"five_hour":{"utilization":45,"resets_at":"2026-10-31T13:00:00.013669+00:00"},"seven_day":{"utilization":80},"extra_usage":{"is_enabled":true,"used_credits":1234,"monthly_limit":5000}});
    let fetcher = ClaudeFetcher::new(dir.path().into(), Arc::new(MockTransport(response)))
        .with_credential_reader(Arc::new(|home| {
            std::fs::read(home.join(".credentials.json"))
                .map_err(|_| RateLimitErrorCode::CredentialsMissing)
        }));
    let result = fetcher.fetch(Utc::now()).await.unwrap();
    assert_eq!(result.plan_type.as_deref(), Some("max"));
    assert_eq!(result.windows[0].remaining_percent, 55.0);
    assert_eq!(result.credits.unwrap().used, Some(12.34));
    std::fs::write(
        path,
        json!({"claudeAiOauth":{"accessToken":"test-token","expiresAt":0}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        fetcher.fetch(Utc::now()).await.unwrap_err(),
        RateLimitErrorCode::TokenExpired
    );
}
#[tokio::test]
async fn codex_live_and_reverse_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("auth.json"),
        json!({"tokens":{"access_token":"test-token","account_id":"test-account"}}).to_string(),
    )
    .unwrap();
    let fetcher = CodexFetcher::new(
        dir.path().into(),
        Arc::new(MockTransport(
            json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":20,"limit_window_seconds":604800}},"credits":{"has_credits":true,"balance":"30"}}),
        )),
    );
    let result = fetcher.fetch(Utc::now()).await.unwrap();
    assert_eq!(result.windows[0].kind, RateLimitWindowKind::Weekly);
    assert_eq!(result.credits.unwrap().balance, Some(30.0));
    assert!(ai_usage_core::rate_limits::codex::parse_live(
        &json!({"rate_limit":{"primary_window":{"used_percent":3,"limit_window_seconds":0}}}),
        Utc::now()
    )
    .is_err());
    let sessions = dir.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let line = |percent| {
        json!({"timestamp":"2026-11-26T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":percent,"window_minutes":300}}}}).to_string()
    };
    std::fs::write(
        sessions.join("a.jsonl"),
        format!("{}\n{}\n", line(10), line(25)),
    )
    .unwrap();
    assert_eq!(fetcher.fallback().unwrap().windows[0].used_percent, 25.0);
}
fn observation(now: DateTime<Utc>) -> RateLimitObservation {
    RateLimitObservation {
        plan_type: None,
        observed_at: now,
        windows: vec![RateLimitWindow {
            kind: RateLimitWindowKind::FiveHour,
            window_seconds: 18000,
            used_percent: 45.0,
            remaining_percent: 55.0,
            resets_at: None,
        }],
        credits: None,
    }
}
struct CountingFetcher(AtomicUsize);
impl RateLimitFetcher for CountingFetcher {
    fn fetch(
        &self,
        now: DateTime<Utc>,
    ) -> Pin<Box<dyn Future<Output = Result<RateLimitObservation, RateLimitErrorCode>> + Send + '_>>
    {
        Box::pin(async move {
            let count = self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            if count == 0 {
                Ok(observation(now))
            } else {
                Err(RateLimitErrorCode::NetworkError)
            }
        })
    }
}
#[tokio::test]
async fn service_coalesces_throttles_and_preserves_stale() {
    let fetcher = Arc::new(CountingFetcher(AtomicUsize::new(0)));
    let service =
        RateLimitUsageService::new(fetcher.clone(), fetcher.clone(), fetcher.clone(), None);
    let now = Utc::now();
    let (a, b) = tokio::join!(service.entry(0, now), service.entry(0, now));
    assert_eq!(a.status, RateLimitStatus::Ok);
    assert_eq!(b.status, RateLimitStatus::Ok);
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .entry(0, now + chrono::Duration::seconds(299))
            .await
            .status,
        RateLimitStatus::Ok
    );
    let stale = service.entry(0, now + chrono::Duration::seconds(300)).await;
    assert_eq!(stale.status, RateLimitStatus::Stale);
    let response = ai_usage_core::types::RateLimitsResponse {
        generated_at: now,
        scope: "local".into(),
        providers: vec![stale],
    };
    assert_eq!(
        formatting::ratio_label(PieChartId::Claude5h, Some(&response)),
        "45%"
    );
    assert_eq!(
        formatting::compact_menu_line(PieChartId::Claude5h, Some(&response), now),
        "Claude Code 5h · 55% left · reset time unknown (stale)"
    );
}
#[cfg(unix)]
#[test]
fn gemini_pty_fixture_and_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("agy");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' '{\"product\":\"antigravity\",\"plan_tier\":\"pro\",\"quota\":{\"gemini-5h\":{\"remaining_fraction\":0.55},\"gemini-weekly\":{\"remaining_fraction\":0.8}}}' > status.json\nread line\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = gemini::fetch(&script, Duration::from_secs(2)).unwrap();
    assert_eq!(result.plan_type.as_deref(), Some("pro"));
    assert_eq!(result.windows.len(), 2);
    let pid_file = dir.path().join("pid");
    std::fs::write(&script, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid_file.display())).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(
        gemini::fetch(&script, Duration::from_millis(100)).unwrap_err(),
        RateLimitErrorCode::Timeout
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    let pid: i32 = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[tokio::test]
async fn transport_rejects_redirect() {
    use ai_usage_core::rate_limits::transport::ReqwestRateLimitTransport;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request).await.unwrap();
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let url: &'static str = Box::leak(format!("http://{address}/").into_boxed_str());
    let transport = ReqwestRateLimitTransport::new().unwrap();
    assert_eq!(
        transport.get(url, vec![]).await.unwrap_err(),
        RateLimitErrorCode::HttpError
    );
    server.await.unwrap();
}
#[tokio::test]
async fn codex_and_gemini_throttle_for_one_minute() {
    let a = Arc::new(CountingFetcher(AtomicUsize::new(0)));
    let b = Arc::new(CountingFetcher(AtomicUsize::new(0)));
    let c = Arc::new(CountingFetcher(AtomicUsize::new(0)));
    let service = RateLimitUsageService::new(a, b.clone(), c.clone(), None);
    let now = Utc::now();
    for index in [1, 2] {
        assert_eq!(service.entry(index, now).await.status, RateLimitStatus::Ok);
        assert_eq!(
            service
                .entry(index, now + chrono::Duration::seconds(59))
                .await
                .status,
            RateLimitStatus::Ok
        );
        assert_eq!(
            service
                .entry(index, now + chrono::Duration::seconds(60))
                .await
                .status,
            RateLimitStatus::Stale
        );
    }
    assert_eq!(b.0.load(Ordering::SeqCst), 2);
    assert_eq!(c.0.load(Ordering::SeqCst), 2);
}

#[test]
fn codex_fallback_limits_search_to_newest_ten_files() {
    let dir = tempfile::tempdir().unwrap();
    let event = json!({"timestamp":"2026-11-26T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":11,"window_minutes":300}}}}).to_string();
    std::fs::write(dir.path().join("old.jsonl"), event).unwrap();
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(dir.path().join(format!("new-{i}.jsonl")), "{}\n").unwrap();
    }
    assert!(ai_usage_core::rate_limits::codex::fallback(dir.path()).is_none());
}

#[cfg(unix)]
#[test]
fn gemini_removes_private_working_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("agy");
    let marker = dir.path().join("working-directory");
    let body = format!("#!/bin/sh\npwd > '{}'\nls -ld . > '{}.mode'\nprintf '%s' '{{\"product\":\"antigravity\",\"quota\":{{\"gemini-5h\":{{\"remaining_fraction\":0.5}}}}}}' > status.json\nread line\n", marker.display(), marker.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    gemini::fetch(&script, Duration::from_secs(2)).unwrap();
    let working = std::fs::read_to_string(&marker).unwrap();
    assert!(!std::path::Path::new(working.trim()).exists());
    assert!(std::fs::read_to_string(marker.with_extension("mode")).unwrap().starts_with("drwx------"));
}
}
