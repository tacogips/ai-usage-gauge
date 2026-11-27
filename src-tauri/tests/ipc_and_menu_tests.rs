use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use ai_usage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ai_usage_core::dashboard::MachineDashboardRouter;
use ai_usage_core::paths::AppPaths;
use ai_usage_core::state::StateStore;
use ai_usage_core::types::{
    AppConfiguration, AppState, BudgetSummary, ChartColors, CostSnapshot, LocalSessionSources,
    MachineDescriptor, MachineKind, MachineRegistry, MachineRuntimeReconciler, PieChartId,
    ProviderRateLimits, RateLimitCreditUnit, RateLimitCredits, RateLimitProvider, RateLimitStatus,
    RateLimitsResponse, ResetCycle, SanitizedCollectionError,
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use tempfile::TempDir;

use ai_usage_gauge_app::dialogs::{
    apply_budget_update, apply_custom_hours_update, apply_refresh_interval_update,
    validate_budget_usd, validate_custom_hours, validate_refresh_interval_seconds,
};
use ai_usage_gauge_app::ipc::{
    dispatch_ipc_request, validate_ipc_request, DashboardIpcRequest, MAX_IPC_BODY_BYTES,
};
use ai_usage_gauge_app::launch_at_login::{LaunchAtLoginController, LaunchAtLoginState};
use ai_usage_gauge_app::tray_menu::TrayViewModel;

fn sample_state() -> AppState {
    AppState {
        budget_usd: Some(Decimal::from(50)),
        budget_machine_ids: vec![],
        reset_cycle: ResetCycle::Daily,
        baseline: None,
        refresh_interval_seconds: Some(30),
        menu_bar_pie_charts: vec![PieChartId::Budget],
        menu_bar_ratio_labels: vec![PieChartId::Budget],
        menu_bar_show_cost: true,
        menu_bar_cost_only: false,
    }
}

#[test]
fn ipc_request_validation_accepts_valid_and_rejects_unsafe() {
    let ok = DashboardIpcRequest {
        path: "/api/metrics?machine=all".into(),
        method: "GET".into(),
        body: None,
    };
    assert!(validate_ipc_request(&ok).is_ok());

    for bad in [
        DashboardIpcRequest {
            path: "/index.html".into(),
            method: "GET".into(),
            body: None,
        },
        DashboardIpcRequest {
            path: "/api/health\r\nEvil: 1".into(),
            method: "GET".into(),
            body: None,
        },
        DashboardIpcRequest {
            path: "/api/health#frag".into(),
            method: "GET".into(),
            body: None,
        },
        DashboardIpcRequest {
            path: "/api/health".into(),
            method: "OPTIONS".into(),
            body: None,
        },
        DashboardIpcRequest {
            path: "/api/machines".into(),
            method: "POST".into(),
            body: Some("x".repeat(MAX_IPC_BODY_BYTES + 1)),
        },
    ] {
        assert!(validate_ipc_request(&bad).is_err());
    }
}

struct DummyFetcher;
impl MachineSnapshotFetcher for DummyFetcher {
    fn fetch<'a>(
        &'a self,
        _descriptor: &'a MachineDescriptor,
        _desired_from: Option<NaiveDate>,
        now: DateTime<Utc>,
    ) -> FetchFuture<'a> {
        Box::pin(async move {
            Ok::<CostSnapshot, SanitizedCollectionError>(CostSnapshot {
                generated_at: now,
                active_boundary_at: now,
                cost_since_reset_usd: Decimal::from(12),
                budget: BudgetSummary::new(Decimal::from(12), Some(Decimal::from(50))),
                reset_cycle: ResetCycle::Daily,
                refresh_interval_seconds: 30,
                points: vec![],
                dashboard_metrics: vec![],
                dashboard_sessions: vec![],
                rolling_events: vec![],
            })
        })
    }
}

#[tokio::test]
async fn dispatch_ipc_request_routes_trusted_in_process() {
    let temp = TempDir::new().unwrap();
    let mut env = HashMap::new();
    env.insert(
        "CCUSAGE_GAUGE_HOME".into(),
        temp.path().to_string_lossy().into_owned(),
    );
    let paths = AppPaths::from_env(&env, temp.path());
    let store = MachineSnapshotStore::new();
    let collector = Arc::new(MachineCollector::new(store, Arc::new(DummyFetcher), 30));
    let reg = MachineRegistry {
        schema_version: 3,
        local_session_sources: LocalSessionSources {
            codex_session_dirs: vec![],
            claude_config_dirs: vec![],
            include_default_codex_dir: false,
            include_default_claude_dir: false,
        },
        machines: vec![MachineDescriptor {
            id: "local".into(),
            display_name: "Local".into(),
            kind: MachineKind::Local,
            enabled: true,
            ssh: None,
            codex_session_dirs: vec![],
            claude_config_dirs: vec![],
            include_default_codex_dir: false,
            include_default_claude_dir: false,
        }],
    };
    collector.reconcile_registry(&reg, 1).await.unwrap();
    collector.refresh_machine("local", None, Utc::now()).await;

    let config = AppConfiguration {
        ccusage_path: None,
        default_reset_term: "daily".into(),
        dashboard_port: 18081,
        dashboard_autostart: false,
        poll_interval_seconds: 20,
        cache_retention_days: 365,
        remote_retry_count: 3,
        remote_retry_delay_seconds: 2,
        remote_timeout_seconds: 15,
        chart_colors: ChartColors::default(),
    };
    let router = MachineDashboardRouter::new(
        paths,
        config,
        sample_state(),
        collector,
        None,
        chrono_tz::UTC,
    );

    let health = dispatch_ipc_request(
        &router,
        DashboardIpcRequest {
            path: "/api/health".into(),
            method: "GET".into(),
            body: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(health.status, 200);
    let parsed: Value = serde_json::from_str(&health.body).unwrap();
    assert_eq!(parsed["status"], "ok");

    // Trusted in-process PATCH succeeds without HTTP mutation header.
    let patch = dispatch_ipc_request(
        &router,
        DashboardIpcRequest {
            path: "/api/machines/local".into(),
            method: "PATCH".into(),
            body: Some(r#"{"includeDefaultCodexDir":false}"#.into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(patch.status, 200);
}

#[test]
fn tray_view_model_formats_titles_tooltips_and_sole_icon_invariant() {
    let state = sample_state();
    let now = Utc::now();
    let snap = CostSnapshot {
        generated_at: now,
        active_boundary_at: now,
        cost_since_reset_usd: Decimal::from(12),
        budget: BudgetSummary::new(Decimal::from(12), Some(Decimal::from(50))),
        reset_cycle: ResetCycle::Daily,
        refresh_interval_seconds: 30,
        points: vec![],
        dashboard_metrics: vec![],
        dashboard_sessions: vec![],
        rolling_events: vec![],
    };
    let rl = RateLimitsResponse {
        generated_at: now,
        scope: "local".into(),
        providers: vec![ProviderRateLimits {
            provider: RateLimitProvider::Claude,
            status: RateLimitStatus::Ok,
            error_code: None,
            message: None,
            plan_type: Some("max".into()),
            fetched_at: Some(now),
            stale: false,
            five_hour: None,
            weekly: None,
            other_windows: vec![],
            credits: Some(RateLimitCredits {
                unit: RateLimitCreditUnit::Usd,
                enabled: true,
                unlimited: false,
                used: Some(10.0),
                limit: Some(20.0),
                balance: None,
                used_percent: Some(50.0),
            }),
        }],
    };

    let vm_ok = TrayViewModel::build(&state, Some(&snap), Some(&rl), None, false, 20, false);
    assert_eq!(vm_ok.status_title, "$12");
    assert!(vm_ok.tooltip.contains("Claude Code credit: 50%"));
    assert_eq!(vm_ok.dashboard_toggle_label, "Open dashboard window");
    // Budget is sole remaining checked icon -> disabled.
    let budget_item = vm_ok
        .usage_icon_items
        .iter()
        .find(|i| i.id == PieChartId::Budget)
        .unwrap();
    assert!(budget_item.checked);
    assert!(!budget_item.enabled);

    let vm_warn = TrayViewModel::build(&state, Some(&snap), None, Some("oops"), true, 20, false);
    assert_eq!(vm_warn.status_title, "$12 !");
    assert_eq!(vm_warn.dashboard_toggle_label, "Close dashboard");

    let vm_none = TrayViewModel::build(&state, None, None, None, false, 20, false);
    assert_eq!(vm_none.status_title, "$—");

    let vm_err_none = TrayViewModel::build(&state, None, None, Some("err"), false, 20, false);
    assert_eq!(vm_err_none.status_title, "$!");

    // `menu_bar_cost_only: true` hides only the budget percentage text; vendor usage icons remain visible.
    let mut cost_only_state = sample_state();
    cost_only_state.menu_bar_cost_only = true;
    cost_only_state.menu_bar_pie_charts = vec![
        PieChartId::ClaudeWeekly,
        PieChartId::CodexWeekly,
        PieChartId::GeminiWeekly,
    ];
    cost_only_state.menu_bar_ratio_labels = vec![PieChartId::Budget, PieChartId::ClaudeWeekly];
    let vm_cost_only =
        TrayViewModel::build(&cost_only_state, Some(&snap), Some(&rl), None, false, 20, false);
    assert!(vm_cost_only
        .render_state
        .entries
        .iter()
        .all(|e| e.id != PieChartId::Budget));
    assert_eq!(vm_cost_only.render_state.entries.len(), 3);
    assert!(vm_cost_only.render_state.entries.iter().all(|e| e.show_icon));
}

#[test]
fn launch_at_login_mock_toggles_cleanly() {
    std::env::set_var("CCUSAGE_GAUGE_E2E_OPEN_MENU", "1");
    assert_eq!(
        LaunchAtLoginController::set_enabled(true).unwrap(),
        LaunchAtLoginState::On
    );
    assert_eq!(LaunchAtLoginController::status(), LaunchAtLoginState::On);
    assert_eq!(
        LaunchAtLoginController::set_enabled(false).unwrap(),
        LaunchAtLoginState::Off
    );
    std::env::remove_var("CCUSAGE_GAUGE_E2E_OPEN_MENU");
}

#[test]
fn dialog_validation_and_persistence() {
    let temp = TempDir::new().unwrap();
    let store = StateStore::new(temp.path().join("state.json"));
    let state = sample_state();

    assert!(validate_budget_usd(Some(Decimal::from(-1))).is_err());
    assert!(validate_custom_hours(0).is_err());
    assert!(validate_refresh_interval_seconds(Some(0)).is_err());

    let s1 = apply_budget_update(&store, &state, Some(Decimal::from(75))).unwrap();
    assert_eq!(s1.budget_usd, Some(Decimal::from(75)));

    let s2 = apply_custom_hours_update(&store, &s1, 6, chrono_tz::UTC).unwrap();
    assert_eq!(s2.reset_cycle, ResetCycle::CustomHours(6));

    let s3 = apply_refresh_interval_update(&store, &s2, Some(15)).unwrap();
    assert_eq!(s3.refresh_interval_seconds, Some(15));
    let _unused: BTreeMap<String, String> = BTreeMap::new();
}
