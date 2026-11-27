use ccusage_core::config::{BootstrapLogger, ConfigStore};
use ccusage_core::state::{
    PieChartSelection, RatioTextSelection, ResetWindowCalculator, StateError, StateStore,
};
use ccusage_core::types::{BudgetSummary, PieChartId, ResetCycle};
use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use rust_decimal::Decimal;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn config_creation_permissions_and_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("config");
    let path = parent.join("config.json");
    let store = ConfigStore::new(path.clone());
    let c = store.load_or_create().unwrap();
    assert_eq!(c.dashboard_port, 18081);
    assert_eq!(
        fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(v["ccusagePath"].is_null());
    v["extra"] = true.into();
    let bytes = serde_json::to_vec(&v).unwrap();
    fs::write(&path, &bytes).unwrap();
    store.load_or_create().unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
#[test]
fn config_validation_keeps_existing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let store = ConfigStore::new(path.clone());
    store.load_or_create().unwrap();
    let base: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (key, value) in [
        ("ccusagePath", serde_json::json!("ccusage")),
        ("dashboardPort", serde_json::json!(0)),
        ("pollIntervalSeconds", serde_json::json!(0)),
        (
            "chartColors",
            serde_json::json!({"light":{"models":{"x":"#ZZZZZZ"}}}),
        ),
    ] {
        let mut v = base.clone();
        v[key] = value;
        let bytes = serde_json::to_vec(&v).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(store
            .load()
            .unwrap_err()
            .to_string()
            .contains("config.json"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let mut v = base;
    v["ccusagePath"] = serde_json::json!("/nonexistent/ccusage");
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(store.load().is_ok());
}
#[test]
fn state_defaults_corruption_and_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/state.json");
    let store = StateStore::new(path.clone());
    let mut state = store.load(ResetCycle::Daily).unwrap();
    assert_eq!(state.menu_bar_pie_charts, vec![PieChartId::Budget]);
    state.budget_usd = Some(Decimal::new(125, 1));
    state.budget_machine_ids = vec!["host".into()];
    state.reset_cycle = ResetCycle::CustomHours(240);
    state.menu_bar_ratio_labels = vec![PieChartId::Claude5h];
    state.menu_bar_show_cost = false;
    state.menu_bar_cost_only = true;
    state = ResetWindowCalculator::new(chrono_tz::UTC)
        .validated_state(
            &state,
            Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap(),
        )
        .unwrap();
    store.save(&state).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(store.load(ResetCycle::Daily).unwrap(), state);
    fs::write(&path, b"invalid").unwrap();
    assert!(matches!(
        store.load(ResetCycle::Daily),
        Err(StateError::Corrupt { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), b"invalid");
}
#[test]
fn reset_windows_dst_and_baselines() {
    let calc = ResetWindowCalculator::new(chrono_tz::America::New_York);
    let now = Utc.with_ymd_and_hms(2026, 3, 8, 16, 30, 0).unwrap();
    let (start, end) = calc.aggregation_interval(ResetCycle::Daily, now).unwrap();
    assert_eq!(end - start, ChronoDuration::hours(23));
    assert_eq!(start, Utc.with_ymd_and_hms(2026, 3, 8, 5, 0, 0).unwrap());
    assert_eq!(
        calc.aggregation_interval(ResetCycle::Hourly, now)
            .unwrap()
            .0,
        Utc.with_ymd_and_hms(2026, 3, 8, 16, 0, 0).unwrap()
    );
    assert_eq!(
        calc.aggregation_interval(ResetCycle::Weekly, now)
            .unwrap()
            .0,
        Utc.with_ymd_and_hms(2026, 3, 2, 5, 0, 0).unwrap()
    );
    assert_eq!(
        calc.aggregation_interval(ResetCycle::Monthly, now)
            .unwrap()
            .0,
        Utc.with_ymd_and_hms(2026, 3, 1, 5, 0, 0).unwrap()
    );
    assert_eq!(
        calc.aggregation_interval(ResetCycle::CustomHours(240), now)
            .unwrap(),
        (now - ChronoDuration::hours(240), now)
    );
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::new(dir.path().join("state"));
    let state = store.load(ResetCycle::Daily).unwrap();
    let state = calc.validated_state(&state, now).unwrap();
    let later = calc
        .validated_state(&state, now + ChronoDuration::minutes(1))
        .unwrap();
    assert_eq!(state.baseline, later.baseline);
    let changed = calc.changing(&state, ResetCycle::Weekly, now).unwrap();
    assert_eq!(changed.baseline.unwrap().cycle, ResetCycle::Weekly);
}
#[test]
fn selection_and_budget() {
    assert_eq!(
        PieChartSelection::toggled(PieChartId::Budget, &[PieChartId::Budget]),
        vec![PieChartId::Budget]
    );
    assert_eq!(
        PieChartSelection::normalized(&[
            PieChartId::Codex5h,
            PieChartId::Budget,
            PieChartId::Budget
        ]),
        vec![PieChartId::Budget, PieChartId::Codex5h]
    );
    assert!(RatioTextSelection::toggled(PieChartId::Budget, &[PieChartId::Budget]).is_empty());
    let b = BudgetSummary::new(Decimal::from(15), Some(Decimal::from(10)));
    assert_eq!(b.overage_usd, Decimal::from(5));
    assert_eq!(b.visual_fraction, Some(Decimal::ONE));
    assert_eq!(b.usage_percentage, Some(Decimal::from(150)));
    assert_eq!(
        BudgetSummary::new(Decimal::from(-1), None).spent_usd,
        Decimal::ZERO
    );
}
fn logger(
    path: std::path::PathBuf,
    fallback: Option<std::path::PathBuf>,
    max: u64,
) -> BootstrapLogger {
    BootstrapLogger::new(
        path,
        fallback,
        "cli",
        || Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap(),
        max,
        Duration::from_secs(72 * 3600),
    )
}
#[test]
fn logger_lazy_rotation_redaction_and_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs");
    let logger = Arc::new(logger(path.clone(), None, 300));
    assert!(!path.exists());
    logger.activate();
    assert!(!path.join("ccusage-gauge.jsonl").exists());
    let mut threads = vec![];
    for _ in 0..4 {
        let log = logger.clone();
        threads.push(std::thread::spawn(move || {
            for _ in 0..8 {
                log.append("error", "startup", "test", "token=secret /private/path");
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let entries: Vec<_> = fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    assert!(entries.len() > 1);
    let mut lines = 0;
    for entry in entries {
        let text = fs::read_to_string(entry).unwrap();
        assert!(!text.contains("secret"));
        assert!(!text.contains("/private"));
        for line in text.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
            lines += 1;
        }
    }
    assert_eq!(lines, 32);
}
#[test]
fn logger_rejects_unsafe_files_and_uses_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("bad");
    fs::write(&primary, b"bad").unwrap();
    let fallback = dir.path().join("fallback");
    let log = logger(primary, Some(fallback.clone()), 10000);
    log.activate();
    assert!(!fallback.join("ccusage-gauge.jsonl").exists());
    log.append("error", "start", "x", "safe");
    assert!(fs::read_to_string(fallback.join("ccusage-gauge.jsonl"))
        .unwrap()
        .contains("log_primary_unavailable"));
    for kind in 0..3 {
        let path = dir.path().join(format!("unsafe{kind}"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let active = path.join("ccusage-gauge.jsonl");
        let target = dir.path().join(format!("target{kind}"));
        fs::write(&target, b"original").unwrap();
        fs::set_permissions(
            &target,
            fs::Permissions::from_mode(if kind == 2 { 0o644 } else { 0o600 }),
        )
        .unwrap();
        if kind == 0 {
            symlink(&target, &active).unwrap();
        } else if kind == 1 {
            fs::hard_link(&target, &active).unwrap();
        } else {
            fs::rename(&target, &active).unwrap();
        }
        let log = logger(path, None, 10000);
        log.append("error", "start", "x", "safe");
        assert_eq!(fs::read(&active).unwrap(), b"original");
    }
}

#[test]
fn logger_retention_preserves_recent_and_removes_expired() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap();
    for (name, age) in [
        ("ccusage-gauge-recent.jsonl", 72 * 3600 - 60),
        ("ccusage-gauge-old.jsonl", 72 * 3600 + 60),
    ] {
        let file = fs::File::create(path.join(name)).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        let modified: SystemTime = UNIX_EPOCH + Duration::from_secs((now.timestamp() - age) as u64);
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
    logger(path.clone(), None, 10000).activate();
    assert!(path.join("ccusage-gauge-recent.jsonl").exists());
    assert!(!path.join("ccusage-gauge-old.jsonl").exists());
}

#[test]
fn logger_redacts_unlabelled_message_and_caps_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs");
    let log = logger(path.clone(), None, 10 * 1024 * 1024);
    let secret = "sk-example-unlabelled-credential";
    log.append("error", "startup", "failure", secret);
    log.append("error", "startup", "failure", &"\"".repeat(100_000));
    let text = fs::read_to_string(path.join("ccusage-gauge.jsonl")).unwrap();
    assert!(!text.contains(secret));
    let records: Vec<_> = text.lines().collect();
    assert_eq!(records.len(), 2);
    for record in records {
        assert!(record.len() < 16 * 1024);
        let value: serde_json::Value = serde_json::from_str(record).unwrap();
        assert_eq!(value["message"], "[redacted]");
    }
}

#[test]
fn logger_rotates_at_ten_mib() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs");
    let log = logger(path.clone(), None, 10 * 1024 * 1024);
    log.activate();
    let active = path.join("ccusage-gauge.jsonl");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&active)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.write_all(&vec![b'x'; 10 * 1024 * 1024]).unwrap();
    drop(file);
    log.append("error", "startup", "failure", "safe");
    assert_eq!(
        fs::metadata(path.join("ccusage-gauge-20261126T120000000Z-0.jsonl"))
            .unwrap()
            .len(),
        10 * 1024 * 1024
    );
    assert!(fs::metadata(active).unwrap().len() < 16 * 1024);
}

#[test]
fn weekly_window_switches_at_monday_midnight() {
    let calc = ResetWindowCalculator::new(chrono_tz::America::New_York);
    let boundary = Utc.with_ymd_and_hms(2026, 3, 9, 4, 0, 0).unwrap();
    let before = calc
        .aggregation_interval(ResetCycle::Weekly, boundary - ChronoDuration::seconds(1))
        .unwrap();
    let after = calc
        .aggregation_interval(ResetCycle::Weekly, boundary)
        .unwrap();
    assert_eq!(before.1, boundary);
    assert_eq!(after.0, boundary);
    assert_eq!(after.1, Utc.with_ymd_and_hms(2026, 3, 16, 4, 0, 0).unwrap());
}

#[test]
fn rotation_cleans_expired_logs_after_activation() {
    use std::time::UNIX_EPOCH;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs");
    let log = logger(path.clone(), None, 1);
    log.activate();
    let now = Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap();
    for (name, age) in [
        ("ccusage-gauge-expired.jsonl", 72 * 3600 + 60),
        ("ccusage-gauge-recent.jsonl", 72 * 3600 - 60),
        ("unrelated.txt", 72 * 3600 + 60),
    ] {
        let file = fs::File::create(path.join(name)).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + Duration::from_secs((now.timestamp() - age) as u64)),
        )
        .unwrap();
    }
    let collision = path.join("ccusage-gauge-20261126T120000000Z-0.jsonl");
    symlink(path.join("missing"), &collision).unwrap();
    log.append("error", "startup", "test", "first");
    log.append("error", "startup", "test", "second");
    assert!(!path.join("ccusage-gauge-expired.jsonl").exists());
    assert!(path.join("ccusage-gauge-recent.jsonl").exists());
    assert!(path.join("unrelated.txt").exists());
    assert!(fs::symlink_metadata(&collision)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(path
        .join("ccusage-gauge-20261126T120000000Z-1.jsonl")
        .exists());
    let active = fs::read_to_string(path.join("ccusage-gauge.jsonl")).unwrap();
    assert!(!active.is_empty());
    let _: serde_json::Value = serde_json::from_str(active.trim()).unwrap();
}

#[test]
fn midnight_gap_and_fall_back_daily_windows() {
    let calc = ResetWindowCalculator::new(chrono_tz::America::Havana);
    let first = Utc.with_ymd_and_hms(2026, 3, 7, 18, 0, 0).unwrap();
    let second = Utc.with_ymd_and_hms(2026, 3, 8, 18, 0, 0).unwrap();
    let a = calc.aggregation_interval(ResetCycle::Daily, first).unwrap();
    let b = calc
        .aggregation_interval(ResetCycle::Daily, second)
        .unwrap();
    assert_eq!(a.0, Utc.with_ymd_and_hms(2026, 3, 7, 5, 0, 0).unwrap());
    assert_eq!(a.1, Utc.with_ymd_and_hms(2026, 3, 8, 5, 0, 0).unwrap());
    assert_eq!(a.1, b.0);
    assert_eq!(b.1, Utc.with_ymd_and_hms(2026, 3, 9, 4, 0, 0).unwrap());
    assert_eq!(a.1 - a.0, ChronoDuration::hours(24));
    assert_eq!(b.1 - b.0, ChronoDuration::hours(23));
    let dir = tempfile::tempdir().unwrap();
    let state = StateStore::new(dir.path().join("state"))
        .load(ResetCycle::Daily)
        .unwrap();
    for (now, start) in [(first, a.0), (second, b.0)] {
        assert_eq!(
            calc.scheduled_boundary(ResetCycle::Daily, now).unwrap(),
            start
        );
        assert_eq!(
            calc.baseline(&state, now).unwrap().active_boundary_at,
            start
        );
        assert_eq!(
            calc.validated_state(&state, now)
                .unwrap()
                .baseline
                .unwrap()
                .active_boundary_at,
            start
        );
    }
    let fall = ResetWindowCalculator::new(chrono_tz::America::New_York)
        .aggregation_interval(
            ResetCycle::Daily,
            Utc.with_ymd_and_hms(2026, 11, 1, 18, 0, 0).unwrap(),
        )
        .unwrap();
    assert_eq!(fall.1 - fall.0, ChronoDuration::hours(25));
}
