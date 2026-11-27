use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::Arc;

use ccusage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ccusage_core::dashboard::{
    resolve_desktop_executable, DashboardAPIClient, DashboardHTTPServer, MachineDashboardRouter,
    RouterRequest, StaticAssetResolver,
};
use ccusage_core::paths::AppPaths;
use ccusage_core::types::{
    AppConfiguration, AppState, BudgetSummary, ChartColors, CostRecord, CostSnapshot,
    LocalSessionSources, MachineDescriptor, MachineKind, MachineRegistry, MachineRuntimeReconciler,
    MetricRecord, PieChartId, PricedUsageEvent, ResetCycle, SanitizedCollectionError,
    SessionMetricRecord, TimestampedUsageEvent, UsageDataQuality,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use tempfile::TempDir;

struct StaticFetcher {
    snapshots: BTreeMap<String, CostSnapshot>,
}

impl MachineSnapshotFetcher for StaticFetcher {
    fn fetch<'a>(
        &'a self,
        descriptor: &'a MachineDescriptor,
        _desired_from: Option<NaiveDate>,
        _now: DateTime<Utc>,
    ) -> FetchFuture<'a> {
        let snap = self.snapshots.get(&descriptor.id).cloned();
        Box::pin(async move {
            snap.ok_or_else(|| SanitizedCollectionError {
                code: "internal_error".into(),
                message: "no snapshot".into(),
                detail: None,
                remediation: None,
            })
        })
    }
}

fn make_event(
    ts: DateTime<Utc>,
    cost: i64,
    dir: Option<&str>,
    effort: Option<&str>,
) -> PricedUsageEvent {
    PricedUsageEvent {
        event: TimestampedUsageEvent {
            identity: ts.to_rfc3339(),
            timestamp: ts,
            agent: "claude".into(),
            session_id: "s1".into(),
            request_id: "r1".into(),
            message_id: "m1".into(),
            model: "test-model".into(),
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_5m_tokens: 0,
            cache_creation_1h_tokens: 0,
            directory: dir.map(str::to_owned),
            effort: effort.map(str::to_owned),
        },
        cost_usd: Decimal::from(cost),
    }
}

async fn build_test_router(temp: &TempDir, cycle: ResetCycle) -> Arc<MachineDashboardRouter> {
    let mut env = HashMap::new();
    env.insert(
        "CCUSAGE_GAUGE_HOME".into(),
        temp.path().to_string_lossy().into_owned(),
    );
    let paths = AppPaths::from_env(&env, temp.path());
    let now = Utc::now();
    let e1 = make_event(
        now - Duration::minutes(30),
        2,
        Some("/repo/a"),
        Some("high"),
    );
    let e2 = make_event(now - Duration::minutes(70), 3, Some("/repo/b"), Some("low"));
    let today = now.format("%Y-%m-%d").to_string();

    let snap = CostSnapshot {
        generated_at: now,
        active_boundary_at: now - Duration::hours(1),
        cost_since_reset_usd: Decimal::from(2),
        budget: BudgetSummary::new(Decimal::from(2), Some(Decimal::from(100))),
        reset_cycle: cycle.clone(),
        refresh_interval_seconds: 30,
        points: vec![CostRecord {
            timestamp: now - Duration::minutes(30),
            cost_usd: Decimal::from(2),
            models: vec!["test-model".into()],
            machine: "local".into(),
            directory: None,
        }],
        dashboard_metrics: vec![MetricRecord {
            date: today,
            agent: "claude".into(),
            model: "test-model".into(),
            cost_usd: Decimal::from(5),
            input_tokens: 20,
            output_tokens: 10,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 30,
            machine: "local".into(),
            directory: None,
            effort: None,
        }],
        dashboard_sessions: vec![
            SessionMetricRecord {
                timestamp: now - Duration::minutes(30),
                agent: "claude".into(),
                model: "test-model".into(),
                cost_usd: Decimal::from(2),
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: 15,
                data_quality: UsageDataQuality::Timestamped,
                machine: "local".into(),
                directory: Some("/repo/a".into()),
                effort: Some("high".into()),
            },
            SessionMetricRecord {
                timestamp: now - Duration::minutes(70),
                agent: "claude".into(),
                model: "test-model".into(),
                cost_usd: Decimal::from(3),
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: 15,
                data_quality: UsageDataQuality::Timestamped,
                machine: "local".into(),
                directory: Some("/repo/b".into()),
                effort: Some("low".into()),
            },
        ],
        rolling_events: vec![e1, e2],
    };

    let mut snaps = BTreeMap::new();
    snaps.insert("local".into(), snap);
    let fetcher = Arc::new(StaticFetcher { snapshots: snaps });
    let store = MachineSnapshotStore::new();
    let collector = Arc::new(MachineCollector::new(store, fetcher, 30));

    let registry = MachineRegistry {
        schema_version: 3,
        local_session_sources: LocalSessionSources {
            codex_session_dirs: vec![],
            claude_config_dirs: vec![],
            include_default_codex_dir: false,
            include_default_claude_dir: false,
        },
        machines: vec![
            MachineDescriptor {
                id: "local".into(),
                display_name: "Local".into(),
                kind: MachineKind::Local,
                enabled: true,
                ssh: None,
                codex_session_dirs: vec![],
                claude_config_dirs: vec![],
                include_default_codex_dir: false,
                include_default_claude_dir: false,
            },
            MachineDescriptor {
                id: "disabled-box".into(),
                display_name: "Disabled".into(),
                kind: MachineKind::Ssh,
                enabled: false,
                ssh: None,
                codex_session_dirs: vec![],
                claude_config_dirs: vec![],
                include_default_codex_dir: false,
                include_default_claude_dir: false,
            },
        ],
    };
    collector.reconcile_registry(&registry, 1).await.unwrap();
    collector.refresh_machine("local", None, now).await;

    let config = AppConfiguration {
        ccusage_path: None,
        default_reset_term: "daily".into(),
        dashboard_port: 17333,
        dashboard_autostart: false,
        poll_interval_seconds: 30,
        cache_retention_days: 365,
        remote_retry_count: 3,
        remote_retry_delay_seconds: 2,
        remote_timeout_seconds: 15,
        chart_colors: ChartColors::default(),
    };
    let state = AppState {
        budget_usd: Some(Decimal::from(100)),
        budget_machine_ids: vec![],
        reset_cycle: cycle,
        baseline: None,
        refresh_interval_seconds: Some(30),
        menu_bar_pie_charts: vec![PieChartId::Budget],
        menu_bar_ratio_labels: vec![PieChartId::Budget],
        menu_bar_show_cost: true,
        menu_bar_cost_only: false,
    };

    Arc::new(MachineDashboardRouter::new(
        paths,
        config,
        state,
        collector,
        None,
        chrono_tz::UTC,
    ))
}

#[tokio::test]
async fn all_read_routes_and_validation_work() {
    let temp = TempDir::new().unwrap();
    let router = build_test_router(&temp, ResetCycle::CustomHours(1)).await;

    let get = |path: &str, query: &str| RouterRequest {
        method: "GET".into(),
        path: path.into(),
        query: query.into(),
        headers: HashMap::new(),
        body: vec![],
        is_trusted_ipc: true,
        expected_port: None,
    };

    assert_eq!(router.route(get("/api/health", "")).await.status, 200);
    assert_eq!(router.route(get("/api/chart-colors", "")).await.status, 200);
    assert_eq!(router.route(get("/api/rate-limits", "")).await.status, 200);
    assert_eq!(
        router.route(get("/api/recent", "hours=12")).await.status,
        200
    );
    assert_eq!(router.route(get("/api/day", "")).await.status, 200);
    assert_eq!(
        router.route(get("/api/period", "range=today")).await.status,
        200
    );
    assert_eq!(
        router
            .route(get("/api/metrics", "range=today"))
            .await
            .status,
        200
    );
    assert_eq!(
        router.route(get("/api/machine-status", "")).await.status,
        200
    );
    assert_eq!(router.route(get("/api/load-status", "")).await.status, 200);
    assert_eq!(
        router.route(get("/api/dashboard-state", "")).await.status,
        200
    );

    // Budget with CustomHours(1): only the 30-min-ago event ($2) is inside [now-1h, now).
    let budget = router.route(get("/api/budget", "")).await;
    assert_eq!(budget.status, 200);
    let bval: Value = serde_json::from_slice(&budget.body).unwrap();
    assert_eq!(bval["budget"]["spentUSD"], json!(2));

    // Cost-series strict boolean & machine selection errors.
    let ok_cs = router
        .route(get(
            "/api/cost-series",
            "range=today&granularity=15min&directoryBreakdown=true&effortBreakdown=true",
        ))
        .await;
    assert_eq!(ok_cs.status, 200);
    assert_eq!(
        router
            .route(get(
                "/api/cost-series",
                "directoryBreakdown=true&directoryBreakdown=false"
            ))
            .await
            .status,
        400
    );
    assert_eq!(
        router
            .route(get("/api/cost-series", "machine=all&machine=local"))
            .await
            .status,
        400
    );
    assert_eq!(
        router
            .route(get("/api/cost-series", "machine=unknown-id"))
            .await
            .status,
        404
    );
    assert_eq!(
        router
            .route(get("/api/cost-series", "machine=disabled-box"))
            .await
            .status,
        409
    );
}

#[tokio::test]
async fn mutation_security_gates_fail_closed_before_body_decode() {
    let temp = TempDir::new().unwrap();
    let router = build_test_router(&temp, ResetCycle::Daily).await;

    // Missing X-CCUsage-Gauge-Mutation header -> 403 even with malformed JSON body.
    let req = RouterRequest {
        method: "PUT".into(),
        path: "/api/subdirectories/name".into(),
        query: String::new(),
        headers: HashMap::from([("host".into(), "127.0.0.1:17333".into())]),
        body: b"{not valid json".to_vec(),
        is_trusted_ipc: false,
        expected_port: Some(17333),
    };
    assert_eq!(router.route(req).await.status, 403);

    // Bad Host -> 403.
    let bad_host = RouterRequest {
        method: "GET".into(),
        path: "/api/health".into(),
        query: String::new(),
        headers: HashMap::from([("host".into(), "evil.example:17333".into())]),
        body: vec![],
        is_trusted_ipc: false,
        expected_port: Some(17333),
    };
    assert_eq!(router.route(bad_host).await.status, 403);

    // Cross-origin Origin -> 403.
    let bad_origin = RouterRequest {
        method: "POST".into(),
        path: "/api/machines".into(),
        query: String::new(),
        headers: HashMap::from([
            ("host".into(), "127.0.0.1:17333".into()),
            ("origin".into(), "http://evil.example".into()),
            ("x-ccusage-gauge-mutation".into(), "1".into()),
        ]),
        body: vec![],
        is_trusted_ipc: false,
        expected_port: Some(17333),
    };
    assert_eq!(router.route(bad_origin).await.status, 403);

    // OPTIONS -> 405.
    let opt = RouterRequest {
        method: "OPTIONS".into(),
        path: "/api/machines".into(),
        query: String::new(),
        headers: HashMap::new(),
        body: vec![],
        is_trusted_ipc: false,
        expected_port: Some(17333),
    };
    assert_eq!(router.route(opt).await.status, 405);

    // GET /api/refresh without mutation header -> 403.
    let refresh = RouterRequest {
        method: "GET".into(),
        path: "/api/refresh".into(),
        query: String::new(),
        headers: HashMap::from([("host".into(), "127.0.0.1:17333".into())]),
        body: vec![],
        is_trusted_ipc: false,
        expected_port: Some(17333),
    };
    assert_eq!(router.route(refresh).await.status, 403);
}

#[tokio::test]
async fn http_server_and_api_client_round_trip_and_rebind() {
    let temp = TempDir::new().unwrap();
    let web_dir = temp.path().join("web");
    fs::create_dir_all(&web_dir).unwrap();
    fs::write(web_dir.join("index.html"), "<html>ok</html>").unwrap();

    let router = build_test_router(&temp, ResetCycle::Daily).await;
    let assets = Arc::new(StaticAssetResolver::resolve(Some(&web_dir), None));

    let handle = DashboardHTTPServer::start(0, router.clone(), assets.clone())
        .await
        .unwrap();
    let port = handle.port();

    let client = DashboardAPIClient::new(port).unwrap();
    let health = client.get_json("/api/health").await.unwrap();
    assert_eq!(health["status"], "ok");

    // Static asset SPA fallback.
    let (_, html) = client
        .request_raw("GET", "/some/spa/route", None, false)
        .await
        .unwrap();
    assert!(html.contains("<html>ok</html>"));

    // Graceful stop and immediate rebind on the exact same port.
    handle.stop().await;
    let missing_assets = Arc::new(StaticAssetResolver::from_root(None));
    let handle2 = DashboardHTTPServer::start(port, router, missing_assets)
        .await
        .unwrap();
    let err = client
        .request_raw("GET", "/", None, false)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    handle2.stop().await;
}

#[test]
fn resolve_desktop_executable_honors_override_without_fallback() {
    let temp = TempDir::new().unwrap();
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let cli_exe = bin_dir.join("ccusage-gauge");
    let sibling_app = bin_dir.join("ccusage-gauge-app");
    let custom_app = temp.path().join("custom-app");
    for p in [&cli_exe, &sibling_app, &custom_app] {
        fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    // 1. Unset env -> resolves sibling ccusage-gauge-app.
    let resolved = resolve_desktop_executable(&HashMap::new(), &cli_exe).unwrap();
    assert_eq!(resolved, fs::canonicalize(&sibling_app).unwrap());

    // 2. Valid explicit override -> wins over sibling.
    let mut env = HashMap::new();
    env.insert(
        "CCUSAGE_GAUGE_DESKTOP_EXECUTABLE".into(),
        custom_app.to_string_lossy().into_owned(),
    );
    let resolved = resolve_desktop_executable(&env, &cli_exe).unwrap();
    assert_eq!(resolved, fs::canonicalize(&custom_app).unwrap());

    // 3. Invalid or empty explicit override -> FAILS WITHOUT falling back to sibling.
    env.insert(
        "CCUSAGE_GAUGE_DESKTOP_EXECUTABLE".into(),
        temp.path()
            .join("missing-app")
            .to_string_lossy()
            .into_owned(),
    );
    assert!(resolve_desktop_executable(&env, &cli_exe).is_err());

    env.insert("CCUSAGE_GAUGE_DESKTOP_EXECUTABLE".into(), String::new());
    assert!(resolve_desktop_executable(&env, &cli_exe).is_err());
}
