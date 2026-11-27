use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;

use ai_usage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ai_usage_core::dashboard::{DashboardHTTPServer, MachineDashboardRouter, StaticAssetResolver};
use ai_usage_core::paths::AppPaths;
use ai_usage_core::types::{
    AppConfiguration, AppState, BudgetSummary, ChartColors, CostSnapshot, LocalSessionSources,
    MachineDescriptor, MachineKind, MachineRegistry, MachineRuntimeReconciler, PieChartId,
    ResetCycle, SanitizedCollectionError,
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use tempfile::TempDir;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-usage-gauge")
}

#[test]
fn help_and_version_create_zero_files_in_isolated_home() {
    let temp = TempDir::new().unwrap();
    for flag in ["--help", "--version"] {
        let out = Command::new(cli_bin())
            .arg(flag)
            .env("CCUSAGE_GAUGE_HOME", temp.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "flag {flag} failed");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

#[test]
fn config_check_creates_default_config_without_ccusage_binary() {
    let temp = TempDir::new().unwrap();
    let out = Command::new(cli_bin())
        .arg("config-check")
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dashboardPort: 18081"));
    assert!(temp
        .path()
        .join(".config/ai-usage-gauge/config.json")
        .is_file());
}

#[test]
fn usage_snapshot_json_works_natively_without_ccusage() {
    let temp = TempDir::new().unwrap();
    let out = Command::new(cli_bin())
        .args(["usage-snapshot", "--json"])
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .env("HOME", temp.path())
        .env("CCUSAGE_GAUGE_ANTIGRAVITY_DIR", "")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["costSinceResetUSD"], 0);
}

#[test]
fn dashboard_command_honors_override_without_fallback() {
    let temp = TempDir::new().unwrap();
    let fake_app = temp.path().join("fake-app.sh");
    fs::write(
        &fake_app,
        "#!/bin/sh\n[ \"$1\" = \"--dashboard-only\" ] && exit 0 || exit 9\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_app, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let ok = Command::new(cli_bin())
        .arg("dashboard")
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .env("CCUSAGE_GAUGE_DESKTOP_EXECUTABLE", &fake_app)
        .output()
        .unwrap();
    assert!(ok.status.success());

    let missing = Command::new(cli_bin())
        .arg("dashboard")
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .env(
            "CCUSAGE_GAUGE_DESKTOP_EXECUTABLE",
            temp.path().join("does-not-exist"),
        )
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("AI_USAGE_GAUGE_DESKTOP_EXECUTABLE"));
}

struct MockFetcher;
impl MachineSnapshotFetcher for MockFetcher {
    fn fetch<'a>(
        &'a self,
        descriptor: &'a MachineDescriptor,
        _desired_from: Option<NaiveDate>,
        now: DateTime<Utc>,
    ) -> FetchFuture<'a> {
        let ok = descriptor.id == "local";
        Box::pin(async move {
            if ok {
                Ok(CostSnapshot {
                    generated_at: now,
                    active_boundary_at: now,
                    cost_since_reset_usd: Decimal::ZERO,
                    budget: BudgetSummary::new(Decimal::ZERO, None),
                    reset_cycle: ResetCycle::Daily,
                    refresh_interval_seconds: 30,
                    points: vec![],
                    dashboard_metrics: vec![],
                    dashboard_sessions: vec![],
                    rolling_events: vec![],
                })
            } else {
                Err(SanitizedCollectionError {
                    code: "transport_failed".into(),
                    message: "offline".into(),
                    detail: None,
                    remediation: None,
                })
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_subcommands_round_trip_and_map_exit_codes() {
    let temp = TempDir::new().unwrap();
    let mut env = std::collections::HashMap::new();
    env.insert(
        "CCUSAGE_GAUGE_HOME".into(),
        temp.path().to_string_lossy().into_owned(),
    );
    let paths = AppPaths::from_env(&env, temp.path());
    let store = MachineSnapshotStore::new();
    let collector = Arc::new(MachineCollector::new(store, Arc::new(MockFetcher), 30));
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
        poll_interval_seconds: 30,
        cache_retention_days: 365,
        remote_retry_count: 3,
        remote_retry_delay_seconds: 2,
        remote_timeout_seconds: 15,
        chart_colors: ChartColors::default(),
    };
    let state = AppState {
        budget_usd: None,
        budget_machine_ids: vec![],
        reset_cycle: ResetCycle::Daily,
        baseline: None,
        refresh_interval_seconds: Some(30),
        menu_bar_pie_charts: vec![PieChartId::Budget],
        menu_bar_ratio_labels: vec![PieChartId::Budget],
        menu_bar_show_cost: true,
        menu_bar_cost_only: false,
    };
    let router = Arc::new(MachineDashboardRouter::new(
        paths,
        config,
        state,
        collector,
        None,
        chrono_tz::UTC,
    ));
    let handle =
        DashboardHTTPServer::start(0, router, Arc::new(StaticAssetResolver::from_root(None)))
            .await
            .unwrap();
    let port = handle.port();

    // Health succeeds with exit 0.
    let health = Command::new(cli_bin())
        .args([
            "client",
            "--port",
            &port.to_string(),
            "--json",
            "dashboard",
            "health",
        ])
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .output()
        .unwrap();
    assert!(health.status.success());
    let val: Value = serde_json::from_slice(&health.stdout).unwrap();
    assert_eq!(val["status"], "ok");

    // Unknown machine -> exit 4.
    let not_found = Command::new(cli_bin())
        .args([
            "client",
            "--port",
            &port.to_string(),
            "machines",
            "show",
            "nope",
        ])
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .output()
        .unwrap();
    assert_eq!(not_found.status.code(), Some(4));

    // Invalid flag -> exit 2.
    let bad_flag = Command::new(cli_bin())
        .args(["client", "--not-a-real-flag"])
        .output()
        .unwrap();
    assert_eq!(bad_flag.status.code(), Some(2));

    handle.stop().await;

    // Stopped server -> exit 3 (unreachable).
    let unreachable = Command::new(cli_bin())
        .args(["client", "--port", &port.to_string(), "dashboard", "health"])
        .env("CCUSAGE_GAUGE_HOME", temp.path())
        .output()
        .unwrap();
    assert_eq!(unreachable.status.code(), Some(3));
    let _unused: BTreeMap<String, String> = BTreeMap::new();
}
