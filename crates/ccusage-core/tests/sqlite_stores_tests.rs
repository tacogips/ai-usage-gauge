use ccusage_core::cache::{
    DashboardDirectoryNameStore, DashboardStateStore, LocalCacheMigrator, MigrationOutcome,
};
use ccusage_core::types::{DashboardUIState, DirectoryNameLifecycle};
use tempfile::tempdir;

#[test]
fn dashboard_state_round_trips_swift_json_blob() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state/dashboard-state.sqlite3");
    let store = DashboardStateStore::new(&path);
    assert!(store.load().unwrap().is_none());
    let state: DashboardUIState = serde_json::from_value(serde_json::json!({
        "range":"today", "customStart":"2026-01-01", "customEnd":"2026-01-01",
        "selectedModels":[], "selectedAgents":[], "granularity":"hourly", "chartMetric":"costUSD"
    }))
    .unwrap();
    store.save(&state, 123.0).unwrap();
    assert_eq!(DashboardStateStore::new(&path).load().unwrap(), Some(state));
    let connection = rusqlite::Connection::open(path).unwrap();
    let kind: String = connection
        .query_row(
            "SELECT typeof(value) FROM dashboard_state WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "blob");
}

#[test]
fn directory_names_survive_reopen_and_obey_deletion_barriers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("state/dashboard-state.sqlite3");
    let store = DashboardDirectoryNameStore::new(&path);
    store
        .set_name("local", "/project", Some("  Project  "), 1.0)
        .unwrap();
    assert_eq!(
        store.names(&["local".into()]).unwrap()["local"]["/project"],
        "Project"
    );
    assert!(store
        .set_name("local", "/project", Some("bad\nname"), 1.0)
        .is_err());
    assert!(store
        .set_name("local", "/project", Some(&"x".repeat(201)), 1.0)
        .is_err());
    store.prepare_delete("local").unwrap();
    assert!(store
        .set_name("local", "/project", Some("blocked"), 1.0)
        .is_err());
    store.rollback_delete("local").unwrap();
    store.reconcile_machine_registry(&["local".into()]).unwrap();
    assert_eq!(
        DashboardDirectoryNameStore::new(&path)
            .names(&["local".into()])
            .unwrap()["local"]["/project"],
        "Project"
    );
    store.prepare_delete("local").unwrap();
    store.commit_delete("local").unwrap();
    store.reconcile_machine_registry(&["local".into()]).unwrap();
    assert!(store.names(&["local".into()]).unwrap().is_empty());
    store
        .set_name("local", "/project", Some("new"), 1.0)
        .unwrap();
    store
        .set_name("local", "/project", Some("  "), 1.0)
        .unwrap();
    assert!(store.names(&["local".into()]).unwrap().is_empty());
}

#[test]
fn legacy_cache_migration_moves_database_and_destination_wins() {
    let dir = tempdir().unwrap();
    let legacy = dir.path().join("aggregates.sqlite3");
    let destination = dir.path().join("aggregates-local.sqlite3");
    drop(ccusage_core::cache::UsageAggregationCache::open(&legacy, "local").unwrap());
    let migrator = LocalCacheMigrator::new(&legacy, &destination);
    assert_eq!(
        migrator.migrate_if_needed().unwrap(),
        MigrationOutcome::Migrated
    );
    assert!(!legacy.exists());
    rusqlite::Connection::open(&legacy).unwrap();
    assert_eq!(
        migrator.migrate_if_needed().unwrap(),
        MigrationOutcome::Conflict
    );
    assert!(destination.exists());
}

#[test]
fn legacy_backfill_preserves_partial_history_and_retries_atomically() {
    use ccusage_core::cache::{CoverageClassification, UsageAggregationCache};
    let dir = tempdir().unwrap();
    let path = dir.path().join("aggregate.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',10,0,0,0,NULL);
        INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(10,'claude','m','10',10,0,0,0,'timestamped',NULL,NULL);",
    )
    .unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::AggregateCoveredProvenanceUnscanned
    );
    let raw: ccusage_core::types::SessionMetricRecord = serde_json::from_value(serde_json::json!({
        "timestamp":"1970-01-01T00:00:11Z", "agent":"claude", "model":"m", "costUSD":4,
        "inputTokens":4, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0,
        "dataQuality":"timestamped", "directory":"/partial"
    }))
    .unwrap();
    db.execute_batch("CREATE TRIGGER fail_marker BEFORE INSERT ON antigravity_coverage_ranges BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
    assert!(cache
        .merge_day(
            "2026-01-01",
            0.0,
            86400.0,
            &[],
            std::slice::from_ref(&raw),
            "source",
            generation
        )
        .is_err());
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::AggregateCoveredProvenanceUnscanned
    );
    db.execute_batch("DROP TRIGGER fail_marker").unwrap();
    cache
        .merge_day(
            "2026-01-01",
            0.0,
            86400.0,
            &[],
            &[raw],
            "source",
            generation,
        )
        .unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::NativeComplete
    );
    let sessions = cache.session_metrics(0.0, 86400.0).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].cost_usd.to_string(), "10");
    assert_eq!(sessions[0].directory, None);
    assert_eq!(
        cache.daily_metrics("2026-01-01").unwrap()[0].input_tokens,
        10
    );
    let new_generation = cache.begin_source_attempt("changed").unwrap();
    assert_ne!(generation, new_generation);
    assert!(cache
        .merge_day("2026-01-01", 0.0, 86400.0, &[], &[], "source", generation)
        .is_err());
}

#[test]
fn legacy_partition_completeness_requires_conserved_tokens() {
    use ccusage_core::cache::{evaluate_legacy_partition_completeness, LegacyPartitionDecision};
    use ccusage_core::types::{MetricRecord, SessionMetricRecord};
    let cached: MetricRecord = serde_json::from_value(serde_json::json!({
        "date":"2026-01-01", "agent":"claude", "model":"m", "costUSD":10,
        "inputTokens":10, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0
    }))
    .unwrap();
    let mut raw: SessionMetricRecord = serde_json::from_value(serde_json::json!({
        "timestamp":"2026-01-01T00:00:00Z", "agent":"claude", "model":"m", "costUSD":4,
        "inputTokens":4, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0,
        "dataQuality":"timestamped"
    }))
    .unwrap();
    assert_eq!(
        evaluate_legacy_partition_completeness(std::slice::from_ref(&cached), &[]),
        LegacyPartitionDecision::Absent
    );
    assert_eq!(
        evaluate_legacy_partition_completeness(
            std::slice::from_ref(&cached),
            std::slice::from_ref(&raw)
        ),
        LegacyPartitionDecision::PartialOrUnverifiable
    );
    raw.input_tokens = 10;
    assert_eq!(
        evaluate_legacy_partition_completeness(&[cached], &[raw]),
        LegacyPartitionDecision::Complete
    );
}

#[test]
fn swift_cache_adds_antigravity_once_without_losing_cached_claude() {
    use ccusage_core::cache::{CoverageClassification, UsageAggregationCache};
    use ccusage_core::types::{MetricRecord, SessionMetricRecord};
    let dir = tempdir().unwrap();
    let path = dir.path().join("aggregates-local.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO directory_coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',10,0,0,0,NULL);
        INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(10,'claude','m','10',10,0,0,0,'timestamped',NULL,NULL);",
    )
    .unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::LegacyCoveredNeedsAntigravity
    );
    let daily: MetricRecord = serde_json::from_value(serde_json::json!({"date":"2026-01-01", "agent":"agy", "model":"g", "costUSD":2, "inputTokens":2, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0})).unwrap();
    let session: SessionMetricRecord = serde_json::from_value(serde_json::json!({"timestamp":"1970-01-01T00:00:12Z", "agent":"agy", "model":"g", "costUSD":2, "inputTokens":2, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0, "dataQuality":"timestamped"})).unwrap();
    cache
        .merge_day(
            "2026-01-01",
            0.0,
            86400.0,
            std::slice::from_ref(&daily),
            std::slice::from_ref(&session),
            "source",
            generation,
        )
        .unwrap();
    cache
        .merge_day(
            "2026-01-01",
            0.0,
            86400.0,
            &[daily],
            &[session],
            "source",
            generation,
        )
        .unwrap();
    drop(cache);
    let reopened = UsageAggregationCache::open(&path, "local").unwrap();
    assert_eq!(
        reopened.classify_day("2026-01-01").unwrap(),
        CoverageClassification::NativeComplete
    );
    assert_eq!(reopened.daily_metrics("2026-01-01").unwrap().len(), 2);
    assert_eq!(reopened.session_metrics(0.0, 86400.0).unwrap().len(), 2);
    assert_eq!(
        reopened
            .daily_metrics("2026-01-01")
            .unwrap()
            .iter()
            .map(|r| r.cost_usd)
            .sum::<rust_decimal::Decimal>()
            .to_string(),
        "12"
    );
}

#[test]
fn fingerprint_adopts_legacy_then_purges_on_change() {
    use ccusage_core::cache::UsageAggregationCache;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO cache_metadata VALUES(1,1,'2026-01-01','2026-01-01',NULL); INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',10,0,0,0,NULL);").unwrap();
    cache.begin_source_attempt("one").unwrap();
    assert_eq!(cache.daily_metrics("2026-01-01").unwrap().len(), 1);
    let fingerprint: String = db
        .query_row(
            "SELECT source_configuration_fingerprint FROM cache_metadata",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fingerprint, "one");
    cache.begin_source_attempt("two").unwrap();
    assert!(cache.daily_metrics("2026-01-01").unwrap().is_empty());
}

#[test]
fn interrupted_clear_restores_staged_database() {
    use ccusage_core::cache::MachineCacheRecovery;
    let dir = tempdir().unwrap();
    let path = dir.path().join("aggregates-local.sqlite3");
    let transaction = dir.path().join(".clear-local-test");
    std::fs::create_dir(&transaction).unwrap();
    std::fs::write(transaction.join("aggregates-local.sqlite3"), b"original").unwrap();
    MachineCacheRecovery::reconcile("local", &path).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"original");
    assert!(!transaction.exists());
}

#[test]
fn complete_legacy_partition_replaces_sessions_with_conserved_totals() {
    use ccusage_core::cache::UsageAggregationCache;
    use ccusage_core::types::SessionMetricRecord;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01'); INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',10,0,0,0,NULL); INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(10,'claude','m','10',10,0,0,0,'timestamped',NULL,NULL);").unwrap();
    let raw: SessionMetricRecord = serde_json::from_value(serde_json::json!({"timestamp":"1970-01-01T00:00:11Z", "agent":"claude", "model":"m", "costUSD":4, "inputTokens":10, "outputTokens":0, "cacheCreationTokens":0, "cacheReadTokens":0, "dataQuality":"timestamped", "directory":"/complete"})).unwrap();
    cache
        .merge_day(
            "2026-01-01",
            0.0,
            86400.0,
            &[],
            &[raw],
            "source",
            generation,
        )
        .unwrap();
    drop(cache);
    let cache = UsageAggregationCache::open(&path, "local").unwrap();
    let sessions = cache.session_metrics(0.0, 86400.0).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].cost_usd.to_string(), "10");
    assert_eq!(sessions[0].input_tokens, 10);
    assert_eq!(sessions[0].directory.as_deref(), Some("/complete"));
}

#[test]
fn swift_absent_logs_survive_failed_backfill_reopen_and_retry() {
    use ccusage_core::cache::{CoverageClassification, UsageAggregationCache};
    let dir = tempdir().unwrap();
    let path = dir.path().join("aggregates-local.sqlite3");
    drop(UsageAggregationCache::open(&path, "local").unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP TABLE antigravity_coverage_ranges; PRAGMA user_version=1;
        INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',1,2,3,4,NULL);
        INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(1767225610,'claude','m','10',1,2,3,4,'timestamped',NULL,NULL);",
    )
    .unwrap();
    drop(db);
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_marker BEFORE INSERT ON antigravity_coverage_ranges BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
    assert!(cache
        .merge_day(
            "2026-01-01",
            1767225600.0,
            1767312000.0,
            &[],
            &[],
            "source",
            generation
        )
        .is_err());
    drop(cache);
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::AggregateCoveredProvenanceUnscanned
    );
    assert_eq!(
        cache.session_metrics(1767225600.0, 1767312000.0).unwrap()[0]
            .cost_usd
            .to_string(),
        "10"
    );
    db.execute_batch("DROP TRIGGER fail_marker").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    cache
        .merge_day(
            "2026-01-01",
            1767225600.0,
            1767312000.0,
            &[],
            &[],
            "source",
            generation,
        )
        .unwrap();
    drop(cache);
    let cache = UsageAggregationCache::open(&path, "local").unwrap();
    let rows = cache.session_metrics(1767225600.0, 1767312000.0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].input_tokens,
            rows[0].output_tokens,
            rows[0].cache_creation_tokens,
            rows[0].cache_read_tokens
        ),
        (1, 2, 3, 4)
    );
    assert_eq!(rows[0].directory, None);
    assert_eq!(
        cache.daily_metrics("2026-01-01").unwrap()[0]
            .cost_usd
            .to_string(),
        "10"
    );
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::NativeComplete
    );
}

#[test]
fn complete_backfill_conserves_all_four_token_categories_across_buckets() {
    use ccusage_core::cache::UsageAggregationCache;
    use ccusage_core::types::SessionMetricRecord;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO daily_metrics VALUES('2026-01-01','codex','m','10',11,22,33,44,NULL);
        INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(1767225610,'codex','m','10',11,22,33,44,'timestamped',NULL,NULL);").unwrap();
    let rows: Vec<SessionMetricRecord> = [
        serde_json::json!({"timestamp":"2026-01-01T00:00:11Z","agent":"codex","model":"m","costUSD":1,"inputTokens":1,"outputTokens":2,"cacheCreationTokens":3,"cacheReadTokens":4,"dataQuality":"timestamped","directory":"/one"}),
        serde_json::json!({"timestamp":"2026-01-01T00:15:11Z","agent":"codex","model":"m","costUSD":3,"inputTokens":10,"outputTokens":20,"cacheCreationTokens":30,"cacheReadTokens":40,"dataQuality":"timestamped","directory":"/two"})
    ].into_iter().map(|v| serde_json::from_value(v).unwrap()).collect();
    cache
        .merge_day(
            "2026-01-01",
            1767225600.0,
            1767312000.0,
            &[],
            &rows,
            "source",
            generation,
        )
        .unwrap();
    drop(cache);
    let cache = UsageAggregationCache::open(&path, "local").unwrap();
    let result = cache.session_metrics(1767225600.0, 1767312000.0).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(
        result
            .iter()
            .map(|r| r.cost_usd)
            .sum::<rust_decimal::Decimal>(),
        rust_decimal::Decimal::from(10)
    );
    assert_eq!(result.iter().map(|r| r.input_tokens).sum::<u64>(), 11);
    assert_eq!(result.iter().map(|r| r.output_tokens).sum::<u64>(), 22);
    assert_eq!(
        result.iter().map(|r| r.cache_creation_tokens).sum::<u64>(),
        33
    );
    assert_eq!(result.iter().map(|r| r.cache_read_tokens).sum::<u64>(), 44);
    assert_eq!(result[0].directory.as_deref(), Some("/one"));
    assert_eq!(result[1].directory.as_deref(), Some("/two"));
}

#[test]
fn priced_events_are_floored_and_keep_attribution_and_tokens() {
    use ccusage_core::cache::events_to_session_buckets;
    use ccusage_core::types::PricedUsageEvent;
    let priced: PricedUsageEvent = serde_json::from_value(serde_json::json!({
        "event":{"identity":"i","timestamp":"2026-01-01T00:14:59Z","agent":"codex","sessionId":"s","requestId":"r","messageId":"m","model":"model","inputTokens":1,"outputTokens":2,"cacheCreationTokens":3,"cacheReadTokens":4,"cacheCreation5mTokens":3,"cacheCreation1hTokens":0,"directory":"/p","effort":"high"},"costUSD":1.25
    })).unwrap();
    let rows = events_to_session_buckets(&[priced], 1767225600.0, 1767312000.0, "local").unwrap();
    assert_eq!(rows[0].timestamp.timestamp(), 1767225600);
    assert_eq!(
        (
            rows[0].input_tokens,
            rows[0].output_tokens,
            rows[0].cache_creation_tokens,
            rows[0].cache_read_tokens
        ),
        (1, 2, 3, 4)
    );
    assert_eq!(rows[0].directory.as_deref(), Some("/p"));
    assert_eq!(rows[0].effort.as_deref(), Some("high"));
    assert_eq!(rows[0].cost_usd.to_string(), "1.25");
}

#[test]
fn open_recovers_interrupted_clear_and_migrates_legacy_cache() {
    use ccusage_core::cache::UsageAggregationCache;
    let dir = tempdir().unwrap();
    let local = dir.path().join("aggregates-local.sqlite3");
    let legacy = dir.path().join("aggregates.sqlite3");
    drop(UsageAggregationCache::open(&legacy, "local").unwrap());
    let db = rusqlite::Connection::open(&legacy).unwrap();
    db.execute_batch(
        "INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',1,2,3,4,NULL)",
    )
    .unwrap();
    drop(db);
    let cache = UsageAggregationCache::open(&local, "local").unwrap();
    assert_eq!(cache.daily_metrics("2026-01-01").unwrap().len(), 1);
    drop(cache);
    assert!(!legacy.exists());
    let transaction = dir.path().join(".clear-local-fixture");
    std::fs::create_dir(&transaction).unwrap();
    std::fs::rename(&local, transaction.join("aggregates-local.sqlite3")).unwrap();
    let cache = UsageAggregationCache::open(&local, "local").unwrap();
    assert_eq!(cache.daily_metrics("2026-01-01").unwrap().len(), 1);
    assert!(!transaction.exists());
}

#[test]
fn swift_antigravity_failure_rolls_back_across_reopen_and_retries() {
    use ccusage_core::cache::{CoverageClassification, UsageAggregationCache};
    use ccusage_core::types::{MetricRecord, SessionMetricRecord};
    let dir = tempdir().unwrap();
    let path = dir.path().join("aggregates-local.sqlite3");
    drop(UsageAggregationCache::open(&path, "local").unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE antigravity_coverage_ranges; PRAGMA user_version=1;
        INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO directory_coverage_ranges VALUES('2026-01-01','2026-01-01');
        INSERT INTO daily_metrics VALUES('2026-01-01','claude','m','10',1,2,3,4,NULL);
        INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES(1767225610,'claude','m','10',1,2,3,4,'timestamped',NULL,NULL);").unwrap();
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let daily: MetricRecord = serde_json::from_value(serde_json::json!({"date":"2026-01-01","agent":"agy","model":"g","costUSD":2,"inputTokens":2,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0})).unwrap();
    let session: SessionMetricRecord = serde_json::from_value(serde_json::json!({"timestamp":"2026-01-01T00:15:00Z","agent":"agy","model":"g","costUSD":2,"inputTokens":2,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0,"dataQuality":"timestamped"})).unwrap();
    db.execute_batch("CREATE TRIGGER fail_marker BEFORE INSERT ON antigravity_coverage_ranges BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
    assert!(cache
        .merge_day(
            "2026-01-01",
            1767225600.0,
            1767312000.0,
            std::slice::from_ref(&daily),
            std::slice::from_ref(&session),
            "source",
            generation
        )
        .is_err());
    drop(cache);
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::LegacyCoveredNeedsAntigravity
    );
    assert_eq!(cache.daily_metrics("2026-01-01").unwrap().len(), 1);
    assert_eq!(
        cache
            .session_metrics(1767225600.0, 1767312000.0)
            .unwrap()
            .len(),
        1
    );
    db.execute_batch("DROP TRIGGER fail_marker").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    cache
        .merge_day(
            "2026-01-01",
            1767225600.0,
            1767312000.0,
            &[daily],
            &[session],
            "source",
            generation,
        )
        .unwrap();
    drop(cache);
    let cache = UsageAggregationCache::open(&path, "local").unwrap();
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::NativeComplete
    );
    assert_eq!(cache.daily_metrics("2026-01-01").unwrap().len(), 2);
    assert_eq!(
        cache
            .session_metrics(1767225600.0, 1767312000.0)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn local_day_boundary_preserves_and_coalesces_shared_bucket() {
    use ccusage_core::cache::{events_to_session_buckets, UsageAggregationCache};
    use ccusage_core::types::PricedUsageEvent;
    let dir = tempdir().unwrap();
    let path = dir.path().join("local.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let event = |time: &str, tokens: u64| -> PricedUsageEvent {
        serde_json::from_value(serde_json::json!({"event":{"identity":"i","timestamp":time,"agent":"codex","sessionId":"s","requestId":"r","messageId":"m","model":"model","inputTokens":tokens,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0,"cacheCreation5mTokens":0,"cacheCreation1hTokens":0},"costUSD":tokens})).unwrap()
    };
    let boundary = 1767226050.0; // 00:07:30 UTC
    let start = boundary - 86400.0;
    let end = boundary + 86400.0;
    let left = events_to_session_buckets(
        &[event("2026-01-01T00:06:00Z", 2)],
        start,
        boundary,
        "local",
    )
    .unwrap();
    let right =
        events_to_session_buckets(&[event("2026-01-01T00:08:00Z", 3)], boundary, end, "local")
            .unwrap();
    assert_eq!(left[0].timestamp, right[0].timestamp);
    cache
        .merge_day(
            "2025-12-31",
            start,
            boundary,
            &[],
            &left,
            "source",
            generation,
        )
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort) VALUES (?1,'claude','legacy','7',7,0,0,0,'timestamped',NULL,NULL)", [1767225600.0]).unwrap();
    cache
        .merge_day(
            "2026-01-01",
            boundary,
            end,
            &[],
            &right,
            "source",
            generation,
        )
        .unwrap();
    cache
        .merge_day(
            "2026-01-01",
            boundary,
            end,
            &[],
            &right,
            "source",
            generation,
        )
        .unwrap();
    cache
        .merge_day(
            "2025-12-31",
            start,
            boundary,
            &[],
            &left,
            "source",
            generation,
        )
        .unwrap();
    let combined = cache.session_metrics(start, end).unwrap();
    assert_eq!(combined.len(), 2);
    assert_eq!(
        combined
            .iter()
            .find(|r| r.agent == "codex")
            .unwrap()
            .input_tokens,
        5
    );
    assert_eq!(
        cache
            .session_metrics_for_day("2025-12-31", start, boundary)
            .unwrap()
            .iter()
            .map(|r| r.input_tokens)
            .sum::<u64>(),
        9
    );
    assert_eq!(
        cache
            .session_metrics_for_day("2026-01-01", boundary, end)
            .unwrap()[0]
            .input_tokens,
        3
    );
    drop(cache);
    let cache = UsageAggregationCache::open(&path, "local").unwrap();
    assert_eq!(cache.session_metrics(start, end).unwrap().len(), 2);
    let remote_path = dir.path().join("remote.sqlite3");
    drop(UsageAggregationCache::open(&remote_path, "remote").unwrap());
    let remote = rusqlite::Connection::open(remote_path).unwrap();
    let count: i64 = remote
        .query_row(
            "SELECT count(*) FROM pragma_table_info('session_metrics') WHERE name='source_day'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

const SHARED_BUCKET: f64 = 1767225600.0;
const DAY_BOUNDARY: f64 = 1767226050.0;
const DAY_START: f64 = DAY_BOUNDARY - 86400.0;
const DAY_END: f64 = DAY_BOUNDARY + 86400.0;

fn boundary_row(agent: &str, tokens: u64) -> ccusage_core::types::SessionMetricRecord {
    serde_json::from_value(serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","agent":agent,"model":"m","costUSD":tokens,"inputTokens":tokens,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0,"dataQuality":"timestamped"})).unwrap()
}

fn insert_boundary_row(db: &rusqlite::Connection, agent: &str, tokens: u64, day: Option<&str>) {
    db.execute("INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,source_day) VALUES (?1,?2,'m',?3,?4,0,0,0,'timestamped',?5)", rusqlite::params![SHARED_BUCKET,agent,tokens.to_string(),tokens,day]).unwrap();
}

fn assert_boundary_totals(
    cache: &ccusage_core::cache::UsageAggregationCache,
    agent: &str,
    tokens: u64,
) {
    let rows = cache.session_metrics(DAY_START, DAY_END).unwrap();
    let row = rows.iter().find(|r| r.agent == agent).unwrap();
    assert_eq!(row.input_tokens, tokens);
    assert_eq!(row.cost_usd, rust_decimal::Decimal::from(tokens));
}

#[test]
fn boundary_uncovered_reverse_order_and_same_day_replacement() {
    use ccusage_core::cache::UsageAggregationCache;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    insert_boundary_row(&db, "codex", 99, Some("2026-01-01"));
    cache
        .merge_day(
            "2026-01-01",
            DAY_BOUNDARY,
            DAY_END,
            &[],
            &[boundary_row("codex", 3)],
            "source",
            generation,
        )
        .unwrap();
    assert_boundary_totals(&cache, "codex", 3);
    cache
        .merge_day(
            "2025-12-31",
            DAY_START,
            DAY_BOUNDARY,
            &[],
            &[boundary_row("codex", 2)],
            "source",
            generation,
        )
        .unwrap();
    assert_boundary_totals(&cache, "codex", 5);
    assert_eq!(
        cache
            .session_metrics_for_day("2026-01-01", DAY_BOUNDARY, DAY_END)
            .unwrap()[0]
            .input_tokens,
        3
    );
}

#[test]
fn boundary_legacy_agy_replacement_rolls_back_and_retries() {
    use ccusage_core::cache::{CoverageClassification, UsageAggregationCache};
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    insert_boundary_row(&db, "agy", 2, Some("2025-12-31"));
    insert_boundary_row(&db, "claude", 7, None);
    insert_boundary_row(&db, "agy", 99, Some("2026-01-01"));
    db.execute_batch("INSERT INTO coverage_ranges VALUES('2026-01-01','2026-01-01'); INSERT INTO directory_coverage_ranges VALUES('2026-01-01','2026-01-01'); CREATE TRIGGER fail_coverage BEFORE INSERT ON antigravity_coverage_ranges BEGIN SELECT RAISE(ABORT, 'forced'); END;").unwrap();
    assert!(cache
        .merge_day(
            "2026-01-01",
            DAY_BOUNDARY,
            DAY_END,
            &[],
            &[boundary_row("agy", 3)],
            "source",
            generation
        )
        .is_err());
    assert_eq!(
        cache.classify_day("2026-01-01").unwrap(),
        CoverageClassification::LegacyCoveredNeedsAntigravity
    );
    assert_boundary_totals(&cache, "agy", 101);
    assert_boundary_totals(&cache, "claude", 7);
    db.execute_batch("DROP TRIGGER fail_coverage").unwrap();
    cache
        .merge_day(
            "2026-01-01",
            DAY_BOUNDARY,
            DAY_END,
            &[],
            &[boundary_row("agy", 3)],
            "source",
            generation,
        )
        .unwrap();
    for reopened in [false, true] {
        if reopened {
            drop(cache);
            cache = UsageAggregationCache::open(&path, "local").unwrap();
        }
        assert_boundary_totals(&cache, "agy", 5);
        assert_boundary_totals(&cache, "claude", 7);
        let left = cache
            .session_metrics_for_day("2025-12-31", DAY_START, DAY_BOUNDARY)
            .unwrap();
        assert_eq!(left.iter().map(|r| r.input_tokens).sum::<u64>(), 9);
        assert_eq!(
            left.iter()
                .map(|r| r.cost_usd)
                .sum::<rust_decimal::Decimal>(),
            rust_decimal::Decimal::from(9)
        );
        let right = cache
            .session_metrics_for_day("2026-01-01", DAY_BOUNDARY, DAY_END)
            .unwrap();
        assert_eq!(right[0].input_tokens, 3);
        assert_eq!(right[0].cost_usd, rust_decimal::Decimal::from(3));
    }
}

#[test]
fn boundary_complete_partition_preserves_next_day() {
    use ccusage_core::cache::UsageAggregationCache;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let generation = cache.begin_source_attempt("source").unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    insert_boundary_row(&db, "codex", 2, None);
    insert_boundary_row(&db, "codex", 3, Some("2026-01-01"));
    db.execute_batch("INSERT INTO coverage_ranges VALUES('2025-12-31','2025-12-31'); INSERT INTO daily_metrics VALUES('2025-12-31','codex','m','2',2,0,0,0,NULL);").unwrap();
    cache
        .merge_day(
            "2025-12-31",
            DAY_START,
            DAY_BOUNDARY,
            &[],
            &[boundary_row("codex", 2)],
            "source",
            generation,
        )
        .unwrap();
    assert_boundary_totals(&cache, "codex", 5);
    assert_eq!(
        cache
            .session_metrics_for_day("2025-12-31", DAY_START, DAY_BOUNDARY)
            .unwrap()[0]
            .input_tokens,
        2
    );
    assert_eq!(
        cache
            .session_metrics_for_day("2026-01-01", DAY_BOUNDARY, DAY_END)
            .unwrap()[0]
            .input_tokens,
        3
    );
}

#[test]
fn populated_swift_v1_database_migrates_idempotently() {
    use ccusage_core::cache::UsageAggregationCache;
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=1;
        CREATE TABLE cache_metadata(created_at REAL NOT NULL,updated_at REAL NOT NULL,cached_from TEXT,cached_through TEXT NOT NULL,source_configuration_fingerprint TEXT);
        CREATE TABLE daily_metrics(date TEXT NOT NULL,agent TEXT NOT NULL,model TEXT NOT NULL,cost_usd TEXT NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,cache_creation_tokens INTEGER NOT NULL,cache_read_tokens INTEGER NOT NULL,directory TEXT);
        CREATE TABLE session_metrics(timestamp REAL NOT NULL,agent TEXT NOT NULL,model TEXT NOT NULL,cost_usd TEXT NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,cache_creation_tokens INTEGER NOT NULL,cache_read_tokens INTEGER NOT NULL,data_quality TEXT NOT NULL,directory TEXT,effort TEXT);
        CREATE TABLE coverage_ranges(since_day TEXT NOT NULL,through_day TEXT NOT NULL,PRIMARY KEY(since_day,through_day));
        CREATE TABLE pending_range_jobs(since_day TEXT NOT NULL,through_day TEXT NOT NULL,PRIMARY KEY(since_day,through_day));
        CREATE TABLE directory_coverage_ranges(since_day TEXT NOT NULL,through_day TEXT NOT NULL,PRIMARY KEY(since_day,through_day));
        INSERT INTO directory_coverage_ranges VALUES('2025-12-31','2025-12-31');
        INSERT INTO session_metrics VALUES(1767225600.25,'claude','m','7',7,0,0,0,'timestamped',NULL,NULL);").unwrap();
    drop(db);
    for _ in 0..2 {
        drop(UsageAggregationCache::open(&path, "local").unwrap());
    }
    let db = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    for sql in ["SELECT count(*) FROM pragma_table_info('session_metrics') WHERE name='source_day'", "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='session_metrics_source_day_idx'", "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='antigravity_coverage_ranges'", "SELECT count(*) FROM directory_coverage_ranges WHERE since_day='2025-12-31' AND through_day='2025-12-31'", "SELECT count(*) FROM session_metrics WHERE timestamp=1767225600.25 AND source_day IS NULL"] {
        assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), 1, "{sql}");
    }
}

#[test]
fn recovery_and_clear_isolate_hyphenated_machine_ids() {
    use ccusage_core::cache::{
        MachineCacheClearCoordinator, MachineCacheRecovery, UsageAggregationCache,
    };
    use std::fs;
    let dir = tempdir().unwrap();
    let local = dir.path().join("aggregates-local.sqlite3");
    let sibling = dir.path().join("aggregates-local-2.sqlite3");
    let pending = dir
        .path()
        .join(".clear-local-2-550e8400-e29b-41d4-a716-446655440000");
    let committed = dir
        .path()
        .join(".clear-local-2-550e8400-e29b-41d4-a716-446655440001");
    fs::create_dir(&pending).unwrap();
    fs::create_dir(&committed).unwrap();
    fs::write(pending.join("aggregates-local-2.sqlite3"), b"staged").unwrap();
    fs::write(committed.join("COMMITTED"), b"committed").unwrap();
    fs::write(&sibling, b"conflict").unwrap();
    MachineCacheRecovery::reconcile("local", &local).unwrap();
    drop(UsageAggregationCache::open(&local, "local").unwrap());
    assert!(pending.exists() && committed.exists());
    assert_eq!(fs::read(&sibling).unwrap(), b"conflict");
    fs::remove_file(&sibling).unwrap();
    MachineCacheRecovery::reconcile("local-2", &sibling).unwrap();
    assert_eq!(fs::read(&sibling).unwrap(), b"staged");
    assert!(!pending.exists() && !committed.exists());
    for suffix in ["-journal", "-shm", "-wal"] {
        fs::write(format!("{}{suffix}", local.display()), b"sidecar").unwrap();
    }
    fs::create_dir(&pending).unwrap();
    fs::write(pending.join("other"), b"sibling").unwrap();
    MachineCacheClearCoordinator::clear("local", &local, None).unwrap();
    assert!(!local.exists());
    for suffix in ["-journal", "-shm", "-wal"] {
        assert!(!std::path::Path::new(&format!("{}{suffix}", local.display())).exists());
    }
    assert_eq!(fs::read(&sibling).unwrap(), b"staged");
    assert_eq!(fs::read(pending.join("other")).unwrap(), b"sibling");
}

#[test]
fn cache_load_uses_shared_types_and_fractional_timestamps() {
    use ccusage_core::cache::{evaluate_legacy_partition_completeness, UsageAggregationCache};
    use ccusage_core::types::{
        AggregationCachePayload, AggregationCacheRange, LegacyPartitionDecision,
    };
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.sqlite3");
    let mut cache = UsageAggregationCache::open(&path, "local").unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute(
        "INSERT INTO cache_metadata VALUES (?1, ?2, ?3, ?4, NULL)",
        rusqlite::params![-0.25, 1.5, "2026-01-01", "2026-01-01"],
    )
    .unwrap();
    let payload: AggregationCachePayload = cache.load(2.0, 1, None).unwrap().unwrap();
    assert_eq!(
        payload.created_at,
        chrono::DateTime::from_timestamp(-1, 750_000_000).unwrap()
    );
    assert_eq!(
        payload.updated_at,
        chrono::DateTime::from_timestamp(1, 500_000_000).unwrap()
    );
    let ranges: &[AggregationCacheRange] = &payload.covered_ranges;
    assert_eq!(ranges.len(), 1);
    let decision: LegacyPartitionDecision = evaluate_legacy_partition_completeness(&[], &[]);
    assert_eq!(decision, LegacyPartitionDecision::Absent);
    db.execute("UPDATE cache_metadata SET updated_at=?1", [f64::INFINITY])
        .unwrap();
    assert!(cache.load(2.0, 1, None).is_err());
}
