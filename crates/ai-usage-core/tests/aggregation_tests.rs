use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ai_usage_core::aggregation::blocks::{build_blocks, BlockBuilder};
use ai_usage_core::aggregation::buckets::aggregate_events;
use ai_usage_core::aggregation::collector::{
    MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ai_usage_core::aggregation::merge::CostSnapshotMerge;
use ai_usage_core::aggregation::rolling::{
    exact_event_cost, partition_cost, selected_period_cost, RollingInputs, RollingPartition,
};
use ai_usage_core::aggregation::snapshot_service::SnapshotService;
use ai_usage_core::pricing::{ModelPricingRates, PricingEngine};
use ai_usage_core::types::{
    AppConfiguration, AppState, BudgetSummary, ChartColors, CostSnapshot, DashboardDataDisposition,
    LocalSessionSources, MachineDescriptor, MachineKind, MachineRegistry, MachineRuntimeReconciler,
    MetricRecord, PieChartId, PricedUsageEvent, ResetCycle, SanitizedCollectionError,
    SessionMetricRecord, TimestampedUsageEvent, UsageDataQuality,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use tempfile::TempDir;

fn event(timestamp: &str, model: &str, cost: i64) -> PricedUsageEvent {
    PricedUsageEvent {
        event: TimestampedUsageEvent {
            identity: timestamp.into(),
            timestamp: timestamp.parse().unwrap(),
            agent: "claude".into(),
            session_id: String::new(),
            request_id: String::new(),
            message_id: String::new(),
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_5m_tokens: 0,
            cache_creation_1h_tokens: 0,
            directory: None,
            effort: None,
        },
        cost_usd: Decimal::from(cost),
    }
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn unit_pricing() -> PricingEngine {
    let mut rates = HashMap::new();
    rates.insert(
        "test-model".to_owned(),
        ModelPricingRates {
            input: Decimal::ONE,
            output: Decimal::ZERO,
            cache_read: Some(Decimal::ZERO),
            cache_creation_5m: Some(Decimal::ZERO),
            cache_creation_1h: Some(Decimal::ZERO),
            ..Default::default()
        },
    );
    PricingEngine::new(rates)
}

fn claude_line(ts: &str, req: &str, msg: &str, input_tokens: u64, cwd: Option<&str>) -> String {
    let cwd_json = cwd.map(|c| format!(r#","cwd":"{c}""#)).unwrap_or_default();
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"s1","requestId":"{req}"{cwd_json},"message":{{"id":"{msg}","role":"assistant","model":"test-model","usage":{{"input_tokens":{input_tokens},"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
    )
}

fn default_config() -> AppConfiguration {
    AppConfiguration {
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
    }
}

fn default_state(cycle: ResetCycle) -> AppState {
    AppState {
        budget_usd: Some(Decimal::from(100)),
        budget_machine_ids: vec![],
        reset_cycle: cycle,
        baseline: None,
        refresh_interval_seconds: Some(30),
        menu_bar_pie_charts: vec![PieChartId::Budget],
        menu_bar_ratio_labels: vec![PieChartId::Budget],
        menu_bar_show_cost: true,
        menu_bar_cost_only: false,
    }
}

fn local_descriptor(claude_dir: &Path) -> MachineDescriptor {
    MachineDescriptor {
        id: "local".into(),
        display_name: "Local".into(),
        kind: MachineKind::Local,
        enabled: true,
        ssh: None,
        codex_session_dirs: vec![],
        claude_config_dirs: vec![claude_dir.to_string_lossy().into_owned()],
        include_default_codex_dir: false,
        include_default_claude_dir: false,
    }
}

#[test]
fn cross_midnight_block_overlaps_requested_range() {
    let events = [
        event("2026-11-24T22:05:00Z", "z", 1),
        event("2026-11-25T01:05:00Z", "a", 2),
    ];
    let blocks = build_blocks(
        &events,
        at("2026-11-25T00:00:00Z"),
        at("2026-11-26T00:00:00Z"),
    );
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].timestamp, at("2026-11-24T22:00:00Z"));
    assert_eq!(blocks[0].cost_usd, Decimal::from(3));
    assert_eq!(blocks[0].models, ["a", "z"]);
}

#[test]
fn five_hour_gap_opens_new_block() {
    let events = [
        event("2026-11-25T00:05:00Z", "a", 1),
        event("2026-11-25T05:05:00Z", "a", 2),
    ];
    let blocks = build_blocks(
        &events,
        at("2026-11-25T00:00:00Z"),
        at("2026-11-26T00:00:00Z"),
    );
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1].timestamp, at("2026-11-25T05:00:00Z"));
}

#[test]
fn continuous_multiday_hourly_chain_regenerative_reset_agrees() {
    // Hourly $1 events at :05 from 2026-11-23T23:05Z through 2026-11-25T01:05Z.
    // Every 5 hours opens a new block: 23:00 (23:05..03:05), 04:00 (04:05..08:05),
    // 09:00, 14:00, 19:00, then 2026-11-25 00:00 (00:05, 01:05 -> $2).
    let mut all_events = Vec::new();
    let mut t = at("2026-11-23T23:05:00Z");
    let end = at("2026-11-25T01:05:00Z");
    while t <= end {
        all_events.push(event(&t.to_rfc3339(), "test-model", 1));
        t += Duration::hours(1);
    }

    let points_from = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let prefix = BlockBuilder::discover_regenerative_prefix(
        points_from,
        chrono_tz::UTC,
        Some(NaiveDate::from_ymd_opt(2026, 11, 20).unwrap()),
        |since, through| -> Result<Vec<PricedUsageEvent>, String> {
            Ok(all_events
                .iter()
                .filter(|e| {
                    let d = e.event.timestamp.date_naive();
                    d >= since && d <= through
                })
                .cloned()
                .collect())
        },
    )
    .unwrap();

    let mut combined = prefix;
    combined.extend(
        all_events
            .iter()
            .filter(|e| e.event.timestamp.date_naive() == points_from)
            .cloned(),
    );

    let narrow = build_blocks(
        &combined,
        at("2026-11-25T00:00:00Z"),
        at("2026-11-26T00:00:00Z"),
    );
    let full = build_blocks(
        &all_events,
        at("2026-11-25T00:00:00Z"),
        at("2026-11-26T00:00:00Z"),
    );
    assert_eq!(narrow, full);
    assert_eq!(narrow.len(), 1);
    assert_eq!(narrow[0].timestamp, at("2026-11-25T00:00:00Z"));
    assert_eq!(narrow[0].cost_usd, Decimal::from(2));
}

#[test]
fn buckets_preserve_local_day_and_directory_attribution() {
    let mut a = event("2026-11-25T14:06:00Z", "model", 1);
    a.event.input_tokens = 3;
    a.event.directory = Some("one".into());
    let mut b = event("2026-11-25T14:08:00Z", "model", 2);
    b.event.input_tokens = 5;
    b.event.directory = Some("two".into());
    let (daily, sessions) = aggregate_events(&[a, b], chrono_tz::Asia::Tokyo).unwrap();
    assert_eq!(daily.len(), 1);
    assert_eq!(daily[0].date, "2026-11-25");
    assert_eq!(daily[0].cost_usd, Decimal::from(3));
    assert_eq!(daily[0].total_tokens, 8);
    assert_eq!(daily[0].directory, None);
    assert_eq!(sessions.len(), 2);
    assert!(sessions
        .iter()
        .all(|s| s.timestamp == at("2026-11-25T14:00:00Z")));
    assert_eq!(sessions.iter().map(|s| s.total_tokens).sum::<u64>(), 8);
}

#[test]
fn buckets_floor_negative_unix_time_and_use_local_date() {
    let events = [event("1969-12-31T23:59:59Z", "model", 1)];
    let (daily, sessions) = aggregate_events(&events, chrono_tz::Asia::Tokyo).unwrap();
    assert_eq!(daily[0].date, "1970-01-01");
    assert_eq!(sessions[0].timestamp, at("1969-12-31T23:45:00Z"));
}

#[test]
fn rolling_boundaries_are_exact_and_independent_of_buckets() {
    let mut events = vec![
        event("2026-11-25T11:06:00Z", "model", 1),
        event("2026-11-25T11:07:00Z", "model", 2),
        event("2026-11-25T11:08:00Z", "model", 3),
        event("2026-11-25T12:07:00Z", "model", 4),
    ];
    events[1].event.directory = Some("selected".into());
    events[2].event.directory = Some("other".into());
    let start = at("2026-11-25T11:07:00Z");
    let end = at("2026-11-25T12:07:00Z");
    assert_eq!(exact_event_cost(&events, start, end, &[]), Decimal::from(5));
    assert_eq!(
        exact_event_cost(&events, start, end, &["selected".into()]),
        Decimal::from(2)
    );
    let (_, sessions) = aggregate_events(&events, chrono_tz::UTC).unwrap();
    assert_eq!(
        sessions
            .iter()
            .filter(|s| s.timestamp == at("2026-11-25T11:00:00Z"))
            .map(|s| s.cost_usd)
            .sum::<Decimal>(),
        Decimal::from(6)
    );
}

#[test]
fn legacy_partition_uses_cached_rows_when_raw_is_partial() {
    let mut old = event("2026-11-24T22:45:00Z", "model", 6);
    old.event.input_tokens = 6;
    old.event.directory = Some("deleted".into());
    let mut surviving = event("2026-11-24T23:15:00Z", "model", 4);
    surviving.event.input_tokens = 4;
    surviving.event.directory = Some("surviving".into());
    let (daily, sessions) = aggregate_events(&[old, surviving.clone()], chrono_tz::UTC).unwrap();
    let raw = [surviving];
    let partition = || RollingPartition {
        raw: &raw,
        cached_daily: &daily,
        cached_sessions: &sessions,
        native_or_today: false,
    };
    let start = at("2026-11-24T22:00:00Z");
    let end = at("2026-11-25T02:00:00Z");
    assert_eq!(
        partition_cost(partition(), start, end, &[]).unwrap(),
        Decimal::from(10)
    );
    assert_eq!(
        partition_cost(partition(), start, end, &["deleted".into()]).unwrap(),
        Decimal::from(6)
    );
    assert_eq!(
        partition_cost(partition(), start, at("2026-11-24T23:15:00Z"), &[]).unwrap(),
        Decimal::from(6)
    );
}

#[test]
fn complete_legacy_partition_conserves_cached_cost_before_filtering() {
    let mut a = event("2026-11-24T22:45:00Z", "model", 1);
    a.event.input_tokens = 1;
    let mut b = event("2026-11-24T23:15:00Z", "model", 3);
    b.event.input_tokens = 3;
    let raw = [a, b];
    let (mut daily, sessions) = aggregate_events(&raw, chrono_tz::UTC).unwrap();
    daily[0].cost_usd = Decimal::from(8);
    let partition = RollingPartition {
        raw: &raw,
        cached_daily: &daily,
        cached_sessions: &sessions,
        native_or_today: false,
    };
    assert_eq!(
        partition_cost(
            partition,
            at("2026-11-24T23:00:00Z"),
            at("2026-11-25T00:00:00Z"),
            &[]
        )
        .unwrap(),
        Decimal::from(6)
    );
}

#[test]
fn spanning_file_block_anchor_preserves_zero_prefix_reads_and_rebuilds_on_mutation() {
    let temp = TempDir::new().unwrap();
    let claude_root = temp.path().join("claude");
    let projects = claude_root.join("projects");
    fs::create_dir_all(&projects).unwrap();
    let log_file = projects.join("spanning.jsonl");

    // Pre-floor (2026-11-16) and recent (2026-11-25) events in one spanning file.
    // retention_floor for 2026-11-25 is 2026-11-17.
    let initial_lines = [
        claude_line("2026-11-16T10:05:00Z", "r1", "m1", 2, Some("/p")),
        claude_line("2026-11-25T09:05:00Z", "r2", "m2", 3, Some("/p")),
    ];
    fs::write(&log_file, format!("{}\n", initial_lines.join("\n"))).unwrap();

    let cache_path = temp.path().join("aggregates-local.sqlite3");
    let mut svc = SnapshotService::new(cache_path, unit_pricing(), None, HashMap::new());
    let desc = local_descriptor(&claude_root);
    let state = default_state(ResetCycle::Daily);
    let config = default_config();
    let now = at("2026-11-25T12:00:00Z");

    let snap1 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap1.cost_since_reset_usd, Decimal::from(3));
    assert!(svc.block_builder.anchor().is_some());

    // Append a recent-only event dated today (2026-11-25).
    svc.reset_scan_stats();
    let append1 = format!(
        "{}\n",
        claude_line("2026-11-25T10:05:00Z", "r3", "m3", 5, Some("/p"))
    );
    OpenOptions::new()
        .append(true)
        .open(&log_file)
        .unwrap()
        .write_all(append1.as_bytes())
        .unwrap();

    let snap2 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap2.cost_since_reset_usd, Decimal::from(8));
    let stats2 = svc.scan_stats();
    assert_eq!(stats2.files_resumed, 1);
    assert_eq!(stats2.files_rescanned, 0);

    // Second recent-only append: still 0 rescans.
    svc.reset_scan_stats();
    let append2 = format!(
        "{}\n",
        claude_line("2026-11-25T11:05:00Z", "r4", "m4", 4, Some("/p"))
    );
    OpenOptions::new()
        .append(true)
        .open(&log_file)
        .unwrap()
        .write_all(append2.as_bytes())
        .unwrap();

    let snap3 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap3.cost_since_reset_usd, Decimal::from(12));
    assert_eq!(svc.scan_stats().files_rescanned, 0);

    // Now mutate the historical prefix (truncate & rewrite with changed cost) -> must rescan & rebuild.
    svc.reset_scan_stats();
    let mutated = [
        claude_line("2026-11-16T10:05:00Z", "r1", "m1", 9, Some("/p")),
        claude_line("2026-11-25T09:05:00Z", "r2", "m2", 7, Some("/p")),
    ];
    fs::write(&log_file, format!("{}\n", mutated.join("\n"))).unwrap();

    let snap4 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap4.cost_since_reset_usd, Decimal::from(7));
    assert!(svc.scan_stats().files_rescanned >= 1);
}

#[test]
fn long_custom_hours_240_uses_pinned_boundary_and_interior_sqlite() {
    let temp = TempDir::new().unwrap();
    let claude_root = temp.path().join("claude");
    let projects = claude_root.join("projects");
    fs::create_dir_all(&projects).unwrap();

    // At 2026-11-25 12:07 UTC, CustomHours(240) starts 2026-11-15 12:07 UTC (< retention_floor 2026-11-17).
    let nov15_file = projects.join("nov15.jsonl");
    fs::write(
        &nov15_file,
        format!(
            "{}\n{}\n",
            claude_line("2026-11-15T12:06:00Z", "b1", "bm1", 10, Some("/p")),
            claude_line("2026-11-15T12:08:00Z", "b2", "bm2", 2, Some("/p")),
        ),
    )
    .unwrap();
    let nov16_file = projects.join("nov16.jsonl");
    fs::write(
        &nov16_file,
        format!(
            "{}\n",
            claude_line("2026-11-16T10:00:00Z", "i1", "im1", 4, Some("/p"))
        ),
    )
    .unwrap();
    let nov25_file = projects.join("nov25.jsonl");
    fs::write(
        &nov25_file,
        format!(
            "{}\n",
            claude_line("2026-11-25T10:00:00Z", "t1", "tm1", 5, Some("/p"))
        ),
    )
    .unwrap();

    let cache_path = temp.path().join("aggregates-local.sqlite3");
    let mut svc = SnapshotService::new(cache_path, unit_pricing(), None, HashMap::new());
    let desc = local_descriptor(&claude_root);
    let state = default_state(ResetCycle::CustomHours(240));
    let config = default_config();
    let now = at("2026-11-25T12:07:00Z");

    let snap1 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    // Expect: Nov 15 12:08 ($2) + Nov 16 ($4) + Nov 25 ($5) = $11 (Nov 15 12:06 excluded!)
    assert_eq!(snap1.cost_since_reset_usd, Decimal::from(11));

    // Steady-state second poll: unchanged files -> 0 bytes read, same $11 total.
    svc.reset_scan_stats();
    let snap2 = svc
        .refresh_local(&desc, &state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap2.cost_since_reset_usd, Decimal::from(11));
    assert_eq!(svc.scan_stats().bytes_read, 0);

    // Switching cycle to Daily evicts the pinned boundary slot cleanly.
    let daily_state = default_state(ResetCycle::Daily);
    let snap3 = svc
        .refresh_local(&desc, &daily_state, &config, now, chrono_tz::UTC, None)
        .unwrap();
    assert_eq!(snap3.cost_since_reset_usd, Decimal::from(5));
}

#[test]
fn zero_local_ccusage_calls_with_disabled_roots_returns_empty_snapshot() {
    let temp = TempDir::new().unwrap();
    let cache_path = temp.path().join("aggregates-local.sqlite3");
    let mut svc = SnapshotService::new(cache_path, unit_pricing(), None, HashMap::new());
    let desc = MachineDescriptor {
        id: "local".into(),
        display_name: "Local".into(),
        kind: MachineKind::Local,
        enabled: true,
        ssh: None,
        codex_session_dirs: vec![],
        claude_config_dirs: vec![],
        include_default_codex_dir: false,
        include_default_claude_dir: false,
    };
    let snap = svc
        .refresh_local(
            &desc,
            &default_state(ResetCycle::Daily),
            &default_config(),
            at("2026-11-25T12:00:00Z"),
            chrono_tz::UTC,
            None,
        )
        .unwrap();
    assert_eq!(snap.cost_since_reset_usd, Decimal::ZERO);
    assert!(snap.points.is_empty());
}

#[test]
fn selected_period_cost_matches_applying_on_snapshot() {
    let now = at("2026-11-25T12:00:00Z");
    let ev = event("2026-11-25T11:30:00Z", "test-model", 7);
    let (daily, sessions) = aggregate_events(std::slice::from_ref(&ev), chrono_tz::UTC).unwrap();
    let snap = CostSnapshot {
        generated_at: now,
        active_boundary_at: at("2026-11-25T00:00:00Z"),
        cost_since_reset_usd: Decimal::from(7),
        budget: BudgetSummary::new(Decimal::from(7), Some(Decimal::from(100))),
        reset_cycle: ResetCycle::Daily,
        refresh_interval_seconds: 30,
        points: vec![],
        dashboard_metrics: daily.clone(),
        dashboard_sessions: sessions.clone(),
        rolling_events: vec![ev.clone()],
    };
    let new_state = default_state(ResetCycle::Hourly);
    let applied = snap.applying(&new_state, now, chrono_tz::UTC);
    let expected = selected_period_cost(
        &ResetCycle::Hourly,
        (at("2026-11-25T12:00:00Z"), at("2026-11-25T13:00:00Z")),
        &RollingInputs {
            local_raw_events: vec![ev],
            daily_metrics: daily,
            session_metrics: sessions,
            legacy_local_days: Default::default(),
            today: "2026-11-25".into(),
            retention_floor: "2026-11-17".into(),
        },
        &[],
        chrono_tz::UTC,
    )
    .unwrap();
    assert_eq!(applied.cost_since_reset_usd, expected);
    assert_eq!(applied.cost_since_reset_usd, Decimal::ZERO);
}

struct MockFetcher {
    calls: AtomicUsize,
    delay_ms: u64,
    snapshots: BTreeMap<String, Result<CostSnapshot, SanitizedCollectionError>>,
}

impl MachineSnapshotFetcher for MockFetcher {
    fn fetch<'a>(
        &'a self,
        descriptor: &'a MachineDescriptor,
        _desired_from: Option<NaiveDate>,
        _now: DateTime<Utc>,
    ) -> ai_usage_core::aggregation::collector::FetchFuture<'a> {
        let id = descriptor.id.clone();
        let delay = self.delay_ms;
        let outcome = self.snapshots.get(&id).cloned().unwrap_or_else(|| {
            Err(SanitizedCollectionError {
                code: "internal_error".into(),
                message: "missing fixture".into(),
                detail: None,
                remediation: None,
            })
        });
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            outcome
        })
    }
}

fn stub_snapshot(
    now: DateTime<Utc>,
    machine: &str,
    cost: i64,
    session_ts: DateTime<Utc>,
) -> CostSnapshot {
    CostSnapshot {
        generated_at: now,
        active_boundary_at: now - Duration::hours(1),
        cost_since_reset_usd: Decimal::from(cost),
        budget: BudgetSummary::new(Decimal::from(cost), Some(Decimal::from(100))),
        reset_cycle: ResetCycle::Daily,
        refresh_interval_seconds: 30,
        points: vec![],
        dashboard_metrics: vec![MetricRecord {
            date: session_ts.format("%Y-%m-%d").to_string(),
            agent: "claude".into(),
            model: "test-model".into(),
            cost_usd: Decimal::from(cost),
            input_tokens: 10,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 10,
            machine: machine.into(),
            directory: None,
            effort: None,
        }],
        dashboard_sessions: vec![SessionMetricRecord {
            timestamp: session_ts,
            agent: "claude".into(),
            model: "test-model".into(),
            cost_usd: Decimal::from(cost),
            input_tokens: 10,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 10,
            data_quality: UsageDataQuality::Timestamped,
            machine: machine.into(),
            directory: None,
            effort: None,
        }],
        rolling_events: vec![],
    }
}

#[tokio::test]
async fn multi_machine_collector_single_flight_fencing_and_stale_merge() {
    let now = at("2026-11-25T12:00:00Z");
    let local_desc = MachineDescriptor {
        id: "local".into(),
        display_name: "Local".into(),
        kind: MachineKind::Local,
        enabled: true,
        ssh: None,
        codex_session_dirs: vec![],
        claude_config_dirs: vec![],
        include_default_codex_dir: false,
        include_default_claude_dir: false,
    };
    let remote_desc = MachineDescriptor {
        id: "remote-a".into(),
        display_name: "Remote A".into(),
        kind: MachineKind::Ssh,
        enabled: true,
        ssh: None,
        codex_session_dirs: vec![],
        claude_config_dirs: vec![],
        include_default_codex_dir: false,
        include_default_claude_dir: false,
    };
    let registry = MachineRegistry {
        schema_version: 2,
        local_session_sources: LocalSessionSources {
            codex_session_dirs: vec![],
            claude_config_dirs: vec![],
            include_default_codex_dir: false,
            include_default_claude_dir: false,
        },
        machines: vec![local_desc.clone(), remote_desc.clone()],
    };

    let mut snaps = BTreeMap::new();
    snaps.insert(
        "local".into(),
        Ok(stub_snapshot(now, "local", 5, at("2026-11-25T11:30:00Z"))),
    );
    snaps.insert(
        "remote-a".into(),
        Ok(stub_snapshot(
            now,
            "remote-a",
            8,
            at("2026-11-25T11:45:00Z"),
        )),
    );
    let fetcher = Arc::new(MockFetcher {
        calls: AtomicUsize::new(0),
        delay_ms: 40,
        snapshots: snaps,
    });
    let store = MachineSnapshotStore::new();
    let collector = Arc::new(MachineCollector::new(store.clone(), fetcher.clone(), 30));
    collector.reconcile_registry(&registry, 1).await.unwrap();

    // Concurrent calls for the same machine coalesce into 1 fetch (single-flight).
    let (a, b) = tokio::join!(
        collector.refresh_machine("local", None, now),
        collector.refresh_machine("local", None, now)
    );
    assert!(a && b);
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

    collector.refresh_machine("remote-a", None, now).await;

    // Stale-generation fencing: attempting to commit with old generation 0 is rejected.
    let rejected = store
        .commit_success(
            "local",
            0,
            stub_snapshot(now, "local", 999, at("2026-11-25T11:50:00Z")),
            now,
        )
        .await;
    assert!(!rejected);

    // Mark all machines stale after 2 hours: excluded from Current, readable in Historical,
    // and machineLatestEvents still emitted even when all machines are stale.
    let later = now + Duration::hours(2);
    store
        .mark_stale_if_expired(later, Duration::minutes(10))
        .await;
    let inputs = store.inputs().await;
    let state = default_state(ResetCycle::Daily);
    let current = CostSnapshotMerge::merge(
        &inputs,
        &state,
        "all",
        DashboardDataDisposition::Current,
        later,
        chrono_tz::UTC,
    );
    assert!(current.scope.included_machine_ids.is_empty());
    assert_eq!(current.scope.stale_machine_ids.len(), 2);
    assert_eq!(current.machine_latest_events.len(), 2);
    assert!(current
        .machine_latest_events
        .iter()
        .all(|e| e.latest_event_at.is_some()));

    let historical = CostSnapshotMerge::merge(
        &inputs,
        &state,
        "all",
        DashboardDataDisposition::Historical,
        later,
        chrono_tz::UTC,
    );
    assert_eq!(historical.scope.included_machine_ids.len(), 2);
    assert_eq!(historical.snapshot.dashboard_sessions.len(), 2);
}
