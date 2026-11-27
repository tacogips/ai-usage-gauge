use ccusage_core::{
    paths::AppPaths,
    types::{ResetCycle, UsageDataQuality, UsageEffort},
};
use std::{collections::HashMap, path::Path};

#[test]
fn paths_respect_overrides_and_machine_slugs() {
    let mut env = HashMap::new();
    let paths = AppPaths::from_env(&env, Path::new("/home/test"));
    assert_eq!(
        paths.config_file,
        Path::new("/home/test/.config/ccusage-gauge/ccusage-config.json")
    );
    assert_eq!(
        paths.aggregation_cache_file_for_machine("local").unwrap(),
        Path::new("/home/test/.cache/ccusage-gauge/aggregates-local.sqlite3")
    );
    for invalid in ["", "../escape", "bad/id", "ABC", "bad\\id"] {
        assert!(paths.aggregation_cache_file_for_machine(invalid).is_err());
    }
    env.insert("CCUSAGE_GAUGE_HOME".into(), "/base".into());
    env.insert("CCUSAGE_GAUGE_CONFIG_HOME".into(), "/cfg".into());
    env.insert("CCUSAGE_GAUGE_STATE_HOME".into(), "/state".into());
    env.insert("CCUSAGE_GAUGE_CACHE_HOME".into(), "/cache".into());
    let paths = AppPaths::from_env(&env, Path::new("/home/test"));
    assert_eq!(
        paths.config_file,
        Path::new("/cfg/ccusage-gauge/ccusage-config.json")
    );
    assert_eq!(paths.log_dir, Path::new("/state/ccusage-gauge/logs"));
    assert_eq!(
        paths.dashboard_state_file,
        Path::new("/cache/ccusage-gauge/dashboard-state.sqlite3")
    );
}
#[test]
fn basic_wire_contracts() {
    assert_eq!(
        serde_json::to_string(&UsageDataQuality::SessionEstimated).unwrap(),
        "\"sessionEstimated\""
    );
    assert_eq!(
        serde_json::to_string(&ResetCycle::CustomHours(240)).unwrap(),
        "{\"type\":\"customHours\",\"hours\":240}"
    );
    assert!(serde_json::from_str::<ResetCycle>("{\"type\":\"customHours\",\"hours\":0}").is_err());
    assert_eq!(UsageEffort::normalize(" HIGH ").as_deref(), Some("high"));
    assert_eq!(UsageEffort::normalize("bad value"), None);
}

#[test]
fn home_override_and_independent_roots() {
    let mut env = HashMap::new();
    env.insert("CCUSAGE_GAUGE_HOME".into(), "/alternate".into());
    let paths = AppPaths::from_env(&env, Path::new("/home/test"));
    assert_eq!(
        paths.config_dir,
        Path::new("/alternate/.config/ccusage-gauge")
    );
    assert_eq!(
        paths.state_dir,
        Path::new("/alternate/.local/ccusage-gauge")
    );
    assert_eq!(
        paths.cache_dir,
        Path::new("/alternate/.cache/ccusage-gauge")
    );
    assert_eq!(
        paths.legacy_aggregation_cache_file,
        paths.cache_dir.join("aggregates.sqlite3")
    );
    assert_eq!(
        paths.litellm_pricing_cache_file,
        paths.cache_dir.join("litellm-pricing.json")
    );
    assert_eq!(
        paths
            .aggregation_cache_file_for_machine("prod-box-1")
            .unwrap(),
        paths.cache_dir.join("aggregates-prod-box-1.sqlite3")
    );
    env.insert("CCUSAGE_GAUGE_STATE_HOME".into(), "/state".into());
    let paths = AppPaths::from_env(&env, Path::new("/home/test"));
    assert_eq!(
        paths.state_file,
        Path::new("/state/ccusage-gauge/state.json")
    );
    assert_eq!(
        paths.config_dir,
        Path::new("/alternate/.config/ccusage-gauge")
    );
}

#[test]
fn metric_and_session_decimal_and_totals_match_wire() {
    use ccusage_core::types::{MetricRecord, SessionMetricRecord};
    use serde_json::{json, Value};
    let input = json!({"date":"2026-11-26", "agent":"claude", "model":"m", "costUSD":"0.1234567890123456789", "inputTokens":2, "outputTokens":3, "cacheCreationTokens":4, "cacheReadTokens":5, "totalTokens":999, "effort":" HIGH "});
    let mut invalid_total = input.clone();
    invalid_total["totalTokens"] = json!("ignored");
    assert_eq!(
        serde_json::from_value::<MetricRecord>(invalid_total)
            .unwrap()
            .total_tokens,
        14
    );
    let metric: MetricRecord = serde_json::from_value(input).unwrap();
    assert_eq!(metric.total_tokens, 14);
    assert_eq!(metric.machine, "local");
    assert_eq!(metric.effort.as_deref(), Some("high"));
    let output = serde_json::to_value(&metric).unwrap();
    assert_eq!(output["totalTokens"], 14);
    assert_eq!(output["costUSD"].to_string(), "0.1234567890123456789");
    assert!(output.get("directory").is_none());
    let mut bad = output.clone();
    bad["machine"] = json!("");
    assert!(serde_json::from_value::<MetricRecord>(bad).is_err());
    let mut session: Value = output;
    session.as_object_mut().unwrap().remove("date");
    session["timestamp"] = json!("2026-11-26T00:00:00Z");
    session["dataQuality"] = json!("sessionEstimated");
    session["totalTokens"] = json!(0);
    let session: SessionMetricRecord = serde_json::from_value(session).unwrap();
    assert_eq!(session.total_tokens, 14);
    let output = serde_json::to_value(session).unwrap();
    assert_eq!(output["totalTokens"], 14);
    assert_eq!(output["dataQuality"], "sessionEstimated");
    assert!(output["costUSD"].is_number());
}

#[test]
fn reset_pie_and_chart_color_wire_contracts() {
    use ccusage_core::types::{ChartColors, PieChartId};
    use serde_json::json;
    for (cycle, label) in [
        (ResetCycle::Hourly, "hourly"),
        (ResetCycle::Daily, "daily"),
        (ResetCycle::Weekly, "weekly"),
        (ResetCycle::Monthly, "monthly"),
    ] {
        assert_eq!(serde_json::to_value(&cycle).unwrap(), json!({"type":label}));
        assert_eq!(
            serde_json::from_value::<ResetCycle>(json!({"type":label})).unwrap(),
            cycle
        );
    }
    let labels = [
        "budget",
        "claude5h",
        "claudeWeekly",
        "claudeCredit",
        "codex5h",
        "codexWeekly",
        "codexCredit",
        "gemini5h",
        "geminiWeekly",
    ];
    for (id, label) in PieChartId::ALL.iter().zip(labels) {
        assert_eq!(serde_json::to_value(id).unwrap(), json!(label));
    }
    assert_eq!(PieChartId::menu_bar_cases().len(), 7);
    assert!(PieChartId::menu_bar_cases()
        .iter()
        .all(|id| !id.is_credit()));
    let colors: ChartColors =
        serde_json::from_value(json!({"machines":{"local":"red"},"models":{}})).unwrap();
    assert_eq!(colors.light, colors.dark);
    let colors: ChartColors =
        serde_json::from_value(json!({"light":{"machines":{"local":"red"}}})).unwrap();
    assert!(colors.dark.machines.is_empty());
}

#[test]
fn ssh_and_machine_wire_contracts() {
    use ccusage_core::types::{
        MachineLatestEvent, SSHJumpProxy, SSHProxy, SanitizedCollectionError,
    };
    use serde_json::json;
    assert_eq!(
        serde_json::to_value(SSHProxy::Direct).unwrap(),
        json!({"kind":"direct"})
    );
    let jump = SSHProxy::Jump(SSHJumpProxy {
        host: "bastion".into(),
        port: 22,
        user: "ops".into(),
        identity_file: None,
        known_hosts_file: Some("/etc/ssh/known_hosts".into()),
    });
    let value = json!({"kind":"jump","host":"bastion","port":22,"user":"ops","knownHostsFile":"/etc/ssh/known_hosts"});
    assert_eq!(serde_json::to_value(&jump).unwrap(), value);
    assert_eq!(serde_json::from_value::<SSHProxy>(value).unwrap(), jump);
    assert_eq!(
        serde_json::to_value(SSHProxy::Command {
            executable: "/usr/bin/nc".into()
        })
        .unwrap(),
        json!({"kind":"command","executable":"/usr/bin/nc"})
    );
    let error = SanitizedCollectionError {
        code: "timeout".into(),
        message: "failed".into(),
        detail: Some("detail".into()),
        remediation: Some("retry".into()),
    };
    assert_eq!(serde_json::to_value(error).unwrap()["remediation"], "retry");
    let event = MachineLatestEvent {
        machine: "local".into(),
        latest_event_at: None,
        marker_state: "none".into(),
        in_last_hour: false,
        data_quality: Some(UsageDataQuality::Timestamped),
    };
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["dataQuality"], "timestamped");
    assert_eq!(value["inLastHour"], false);
    assert!(value.get("latestEventAt").is_none());
}

#[test]
fn rate_limit_wire_round_trips_and_explicit_nulls() {
    use ccusage_core::types::{ProviderRateLimits, RateLimitObservation, RateLimitsResponse};
    use serde_json::json;
    let window = json!({"kind":"fiveHour","windowSeconds":18000,"usedPercent":25.0,"remainingPercent":75.0,"resetsAt":null});
    let credits = json!({"unit":"usd","enabled":true,"unlimited":false,"used":null,"limit":null,"balance":null,"usedPercent":null});
    let observation = json!({"planType":"pro","observedAt":"2026-11-26T00:00:00Z","windows":[window.clone()],"credits":credits.clone()});
    let parsed: RateLimitObservation = serde_json::from_value(observation.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), observation);
    let provider = json!({"provider":"claude","status":"stale","errorCode":"token_expired","message":null,"planType":null,"fetchedAt":null,"stale":true,"fiveHour":window,"weekly":null,"otherWindows":[],"credits":credits});
    let parsed: ProviderRateLimits = serde_json::from_value(provider.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), provider);
    let response =
        json!({"generatedAt":"2026-11-26T00:00:00Z","scope":"local","providers":[provider]});
    let parsed: RateLimitsResponse = serde_json::from_value(response.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), response);
}

#[test]
fn snapshot_omits_internal_events_and_optional_budget_values() {
    use ccusage_core::types::CostSnapshot;
    use serde_json::json;
    let input = json!({"generatedAt":"2026-11-26T00:00:00Z", "activeBoundaryAt":"2026-11-26T00:00:00Z", "costSinceResetUSD":"1.25", "budget":{"spentUSD":1.25,"overageUSD":0}, "resetCycle":{"type":"daily"}, "refreshIntervalSeconds":20, "points":[], "dashboardMetrics":[], "dashboardSessions":[]});
    let snapshot: CostSnapshot = serde_json::from_value(input).unwrap();
    let output = serde_json::to_value(snapshot).unwrap();
    assert!(output.get("rollingEvents").is_none());
    assert!(output["budget"].get("budgetUSD").is_none());
    assert_eq!(output["costSinceResetUSD"], json!(1.25));
}

#[test]
fn legacy_state_and_configuration_defaults() {
    use ccusage_core::types::{AppConfiguration, AppState, PieChartId};
    use serde_json::json;
    let state: AppState = serde_json::from_value(json!({"resetCycle":{"type":"daily"},"menuBarPieCharts":["unknown","codex5h","budget","budget"],"menuBarRatioLabels":[]})).unwrap();
    assert_eq!(
        state.menu_bar_pie_charts,
        vec![PieChartId::Budget, PieChartId::Codex5h]
    );
    assert!(!state.menu_bar_show_cost);
    assert!(serde_json::to_value(&state)
        .unwrap()
        .get("budgetUSD")
        .is_none());
    let state: AppState = serde_json::from_value(json!({"resetCycle":{"type":"daily"}})).unwrap();
    assert_eq!(state.menu_bar_pie_charts, vec![PieChartId::Budget]);
    assert!(state.menu_bar_show_cost);
    let config: AppConfiguration = serde_json::from_value(json!({"defaultResetTerm":"daily","dashboardPort":18081,"dashboardAutostart":true,"pollIntervalSeconds":20})).unwrap();
    assert_eq!(config.cache_retention_days, 365);
    assert_eq!(config.remote_retry_count, 3);
    assert_eq!(config.remote_retry_delay_seconds, 2);
    assert_eq!(config.remote_timeout_seconds, 15);
}

#[test]
fn effort_validation_boundaries() {
    for input in [
        "xhigh", "HIGH", "medium", "low", "max", "minimal", "a_b-c", "123",
    ] {
        assert!(UsageEffort::normalize(input).is_some());
    }
    for input in [
        "",
        "  ",
        "bad value",
        "ä",
        "abcdefghijklmnopqrstuvwxyz1234567",
    ] {
        assert_eq!(UsageEffort::normalize(input), None);
    }
    assert_eq!(
        UsageEffort::normalize("abcdefghijklmnopqrstuvwxyz123456")
            .unwrap()
            .len(),
        32
    );
}

#[test]
fn null_chart_scheme_defaults_empty() {
    let colors: ccusage_core::types::ChartColors = serde_json::from_value(
        serde_json::json!({"light":null,"dark":{"machines":{"m":"#ffffff"}}}),
    )
    .unwrap();
    assert!(colors.light.machines.is_empty());
    assert_eq!(colors.dark.machines["m"], "#ffffff");
}

#[test]
fn dashboard_state_legacy_and_normalization() {
    use ccusage_core::types::{DashboardUIState, PieChartId};
    use serde_json::json;
    let mut input = json!({"range":"week","customStart":"2026-11-20","customEnd":"2026-11-26","selectedModels":[],"selectedAgents":[],"granularity":"daily","chartMetric":"costUSD"});
    let state: DashboardUIState = serde_json::from_value(input.clone()).unwrap();
    assert!(state.selected_machines.is_empty());
    assert_eq!(state.stack_by, "model");
    assert_eq!(state.pie_charts, vec![PieChartId::Budget]);
    assert!(state.hidden_pie_ratios.is_empty());
    let output = serde_json::to_value(state).unwrap();
    assert_eq!(output["sidebarCollapsed"], false);
    assert_eq!(output["headerCollapsed"], false);
    input["stackBy"] = json!("bogus");
    input["pieCharts"] = json!(["claudeWeekly", "budget", "bogus", "budget"]);
    input["hiddenPieRatios"] = json!(["codexWeekly", "x", "budget", "budget"]);
    let state: DashboardUIState = serde_json::from_value(input.clone()).unwrap();
    assert_eq!(state.stack_by, "model");
    assert_eq!(
        state.pie_charts,
        vec![PieChartId::Budget, PieChartId::ClaudeWeekly]
    );
    assert_eq!(
        state.hidden_pie_ratios,
        vec![PieChartId::Budget, PieChartId::CodexWeekly]
    );
    input["stackBy"] = json!("modelEffort");
    input["pieCharts"] = json!(["unknown"]);
    input["hiddenPieRatios"] = json!([]);
    let state: DashboardUIState = serde_json::from_value(input).unwrap();
    assert_eq!(state.stack_by, "modelEffort");
    assert_eq!(state.pie_charts, vec![PieChartId::Budget]);
    assert!(state.hidden_pie_ratios.is_empty());
}

#[test]
fn scientific_decimal_wire_round_trips_and_rejects_invalid_values() {
    use ccusage_core::types::{AppState, BudgetSummary, MetricRecord};
    use serde_json::{json, Value};

    let metric_json = r#"{"date":"2026-11-26","agent":"claude","model":"m","costUSD":1e-7,"inputTokens":1,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0}"#;
    let metric: MetricRecord = serde_json::from_str(metric_json).unwrap();
    assert_eq!(
        serde_json::to_value(&metric).unwrap()["costUSD"].to_string(),
        "0.0000001"
    );
    let mut value: Value = serde_json::from_str(metric_json).unwrap();
    value["costUSD"] = json!("2.5E-6");
    let metric: MetricRecord = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(metric).unwrap()["costUSD"].to_string(),
        "0.0000025"
    );
    for invalid in [json!("not-a-number"), json!("1e"), json!(true)] {
        value["costUSD"] = invalid;
        assert!(serde_json::from_value::<MetricRecord>(value.clone()).is_err());
    }

    for token in ["1e-7", "\"2.5E-6\""] {
        let expected = if token == "1e-7" {
            "0.0000001"
        } else {
            "0.0000025"
        };
        let state_json = format!(r#"{{"resetCycle":{{"type":"daily"}},"budgetUSD":{token}}}"#);
        let state: AppState = serde_json::from_str(&state_json).unwrap();
        assert_eq!(
            serde_json::to_value(state).unwrap()["budgetUSD"].to_string(),
            expected
        );
        let budget_json = format!(r#"{{"spentUSD":0,"overageUSD":0,"visualFraction":{token}}}"#);
        let budget_value: Value = serde_json::from_str(&budget_json).unwrap();
        let budget: BudgetSummary = serde_json::from_value(budget_value).unwrap();
        assert_eq!(
            serde_json::to_value(budget).unwrap()["visualFraction"].to_string(),
            expected
        );
    }
    for invalid in [json!("not-a-number"), json!("1e"), json!(true)] {
        assert!(serde_json::from_value::<AppState>(
            json!({"resetCycle":{"type":"daily"},"budgetUSD":invalid})
        )
        .is_err());
    }
}
