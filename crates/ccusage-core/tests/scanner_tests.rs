use ccusage_core::scanner::{codex, LocalUsageScanner, ScanContext};
use ccusage_core::types::ScanFileTransition;
use chrono_tz::UTC;
use std::fs::{self, OpenOptions};
use std::io::Write;

fn claude(ts: &str, id: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"s","requestId":"r","cwd":" /work ","effort":"high","message":{{"id":"{id}","role":"assistant","model":"m","usage":{{"input_tokens":2,"output_tokens":1,"cache_creation":{{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}}}}}}}}"#
    )
}
#[test]
fn incremental_reuse_resume_and_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(&path, format!("{}\n", claude("2026-01-01T12:00:00Z", "a"))).unwrap();
    let mut scanner = LocalUsageScanner::new();
    let scan = |s: &mut LocalUsageScanner| {
        s.scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
            .unwrap()
    };
    let (events, transition) = scan(&mut scanner);
    assert_eq!(transition, ScanFileTransition::Rescan);
    assert_eq!(events[0].directory.as_deref(), Some(" /work "));
    assert_eq!(events[0].cache_creation_5m_tokens, 3);
    scanner.stats.bytes_read = 0;
    assert_eq!(scan(&mut scanner).1, ScanFileTransition::Reuse);
    assert_eq!(scanner.stats.bytes_read, 0);
    let line = claude("2026-01-01T13:00:00Z", "b");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{line}").unwrap();
    assert_eq!(scan(&mut scanner).0.len(), 1);
    writeln!(file).unwrap();
    assert_eq!(scan(&mut scanner).0.len(), 2);
    fs::write(&path, format!("{}\n", claude("2026-01-01T14:00:00Z", "c"))).unwrap();
    assert_eq!(scan(&mut scanner).1, ScanFileTransition::Rescan);
    assert_eq!(
        scanner
            .scan_file(
                &path,
                "2026-01-01",
                chrono_tz::Asia::Tokyo,
                ScanContext::Claude
            )
            .unwrap()
            .1,
        ScanFileTransition::Rescan
    );
}
#[test]
fn codex_forward_context_and_watermark() {
    let mut p = codex::CodexForwardParser::new(Default::default());
    p.consume(br#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"s","cwd":"/work"}}"#, UTC);
    p.consume(br#"{"timestamp":"2026-01-01T00:01:00Z","type":"turn_context","payload":{"model":"m","effort":"high"}}"#, UTC);
    let (_, event) = p.consume(br#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":3,"output_tokens":2},"total_token_usage":{"input_tokens":10,"cached_input_tokens":3,"output_tokens":2,"total_tokens":12}}}}"#, UTC).unwrap();
    assert_eq!((event.input_tokens, event.cache_read_tokens), (7, 3));
    assert_eq!(event.directory.as_deref(), Some("/work"));
    assert_eq!(event.effort.as_deref(), Some("high"));
}
#[test]
fn antigravity_effort_and_token_validation() {
    use ccusage_core::scanner::antigravity::parse_line;
    let event = parse_line(br#"{"step_index":1,"source":"MODEL","created_at":"2026-01-01T00:00:00Z","model":"gemini-xhigh","input_tokens":4}"#, "id", Some("/w")).unwrap();
    assert_eq!(event.model, "gemini");
    assert_eq!(event.effort.as_deref(), Some("xhigh"));
    assert_eq!(event.directory.as_deref(), Some("/w"));
    assert!(parse_line(br#"{"step_index":1,"source":"MODEL","created_at":"2026-01-01T00:00:00Z","model":"m","input_tokens":-1}"#, "id", None).is_none());
}

#[test]
fn codex_reverse_preserves_pre_cutoff_context_and_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let meta = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"s","cwd":"/work"}}"#;
    let turn_a = r#"{"timestamp":"2026-01-01T01:00:00Z","type":"turn_context","payload":{"model":"A","effort":"high"}}"#;
    let turn_b = r#"{"timestamp":"2026-01-02T02:00:00Z","type":"turn_context","payload":{"model":"B","effort":"low"}}"#;
    let token = |ts: &str, count: u32| {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":10,"cached_input_tokens":2}},"total_token_usage":{{"input_tokens":{count}}}}}}}}}"#
        )
    };
    fs::write(
        &path,
        format!(
            "{meta}\n{turn_a}\n{}\n{}\n{turn_b}\n{}\n",
            token("2026-01-01T02:00:00Z", 1),
            token("2026-01-02T01:00:00Z", 2),
            token("2026-01-02T03:00:00Z", 3)
        ),
    )
    .unwrap();
    let context = || {
        ScanContext::Codex(codex::CodexScanContext {
            session_id: "s".into(),
            ..Default::default()
        })
    };
    let mut bounded = LocalUsageScanner::new();
    let (events, _) = bounded
        .scan_file(&path, "2026-01-02", UTC, context())
        .unwrap();
    let mut full = LocalUsageScanner::new();
    let (all, _) = full.scan_file(&path, "", UTC, context()).unwrap();
    for event in &events {
        assert!(all.iter().any(|e| e == event));
        assert_eq!(event.directory.as_deref(), Some("/work"));
    }
    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .any(|e| e.model == "A" && e.effort.as_deref() == Some("high")));
    assert!(events
        .iter()
        .any(|e| e.model == "B" && e.effort.as_deref() == Some("low")));
    writeln!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "{}",
        token("2026-01-02T04:00:00Z", 4)
    )
    .unwrap();
    let (events, transition) = bounded
        .scan_file(&path, "2026-01-02", UTC, context())
        .unwrap();
    assert!(matches!(transition, ScanFileTransition::Resume { .. }));
    assert!(events.iter().any(|e| e.request_id.starts_with("4:")
        && e.model == "B"
        && e.effort.as_deref() == Some("low")));
}

#[test]
fn active_boundary_retains_one_day_and_evicts() {
    use ccusage_core::scanner::file_scan::ScanCacheRetention;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            claude("2026-01-01T12:00:00Z", "a"),
            claude("2026-01-02T12:00:00Z", "b")
        ),
    )
    .unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    let mut retention = ScanCacheRetention::new("2026-01-03".into());
    retention.set_boundary(Some("2026-01-01"));
    retention.insert_boundary(scanner.entries.values().next().unwrap().clone());
    let (_, entries) = retention.active_boundary.as_ref().unwrap();
    assert_eq!(entries.values().next().unwrap().events.len(), 1);
    scanner.entries = entries.clone();
    scanner.stats.bytes_read = 0;
    assert_eq!(
        scanner
            .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
            .unwrap()
            .1,
        ScanFileTransition::Reuse
    );
    assert_eq!(scanner.stats.bytes_read, 0);
    retention.set_boundary(Some("2026-01-02"));
    assert!(retention.active_boundary.as_ref().unwrap().1.is_empty());
    retention.set_boundary(None);
    assert!(retention.active_boundary.is_none());
}

#[test]
fn antigravity_discovery_and_read_only_workspace() {
    use ccusage_core::scanner::antigravity::{transcripts, workspaces};
    let dir = tempfile::tempdir().unwrap();
    let id = "12345678-1234-1234-1234-123456789abc";
    let logs = dir
        .path()
        .join("brain")
        .join(id)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join("transcript.jsonl"), "").unwrap();
    fs::write(logs.join("transcript_full.jsonl"), "").unwrap();
    assert_eq!(
        transcripts(dir.path()).unwrap()[0].1,
        logs.join("transcript.jsonl").canonicalize().unwrap()
    );
    let db = dir.path().join("conversation_summaries.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE conversation_summaries(conversation_id TEXT, workspace_uris TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2)",
            rusqlite::params![id, r#"["file:///work/project"]"#],
        )
        .unwrap();
    drop(connection);
    assert_eq!(
        workspaces(dir.path()).unwrap().get(id).map(String::as_str),
        Some("/work/project")
    );
    assert!(!dir.path().join("conversation_summaries.db-wal").exists());
    assert!(!dir.path().join("conversation_summaries.db-shm").exists());
}

#[test]
fn bounded_codex_scan_stops_before_old_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut file = fs::File::create(&path).unwrap();
    writeln!(file, "{{\"timestamp\":\"2025-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"s\"}}}}").unwrap();
    for _ in 0..2000 {
        writeln!(
            file,
            "{{\"timestamp\":\"2025-01-01T00:00:00Z\",\"type\":\"irrelevant\",\"payload\":{{}}}}"
        )
        .unwrap();
    }
    writeln!(file, "{{\"timestamp\":\"2026-01-02T00:00:00Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"m\"}}}}").unwrap();
    writeln!(file, "{{\"timestamp\":\"2026-01-02T01:00:00Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":1}},\"total_token_usage\":{{\"input_tokens\":1}}}}}}}}").unwrap();
    drop(file);
    let mut scanner = LocalUsageScanner::new();
    let (events, _) = scanner
        .scan_file(
            &path,
            "2026-01-02",
            UTC,
            ScanContext::Codex(Default::default()),
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(scanner.stats.bytes_read < fs::metadata(&path).unwrap().len());
}

#[test]
fn rewrite_boundary_and_timezone_rebucket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let original = format!("{}\n", claude("2026-01-01T23:30:00Z", "a"));
    fs::write(&path, &original).unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner
        .scan_file(&path, "2026-01-02", UTC, ScanContext::Claude)
        .unwrap();
    let (events, transition) = scanner
        .scan_file(
            &path,
            "2026-01-02",
            chrono_tz::Asia::Tokyo,
            ScanContext::Claude,
        )
        .unwrap();
    assert_eq!(transition, ScanFileTransition::Rescan);
    assert_eq!(events.len(), 1);
    let replacement = original.replace("\"id\":\"a\"", "\"id\":\"b\"");
    assert_eq!(replacement.len(), original.len());
    let old_time = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, &replacement).unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_time + std::time::Duration::from_secs(2)))
        .unwrap();
    let (events, transition) = scanner
        .scan_file(
            &path,
            "2026-01-02",
            chrono_tz::Asia::Tokyo,
            ScanContext::Claude,
        )
        .unwrap();
    assert_eq!(transition, ScanFileTransition::Rescan);
    assert_eq!(events[0].message_id, "b");
    fs::write(
        &path,
        format!("{original}{}\n", claude("2026-01-02T01:00:00Z", "c")),
    )
    .unwrap();
    assert_eq!(
        scanner
            .scan_file(
                &path,
                "2026-01-02",
                chrono_tz::Asia::Tokyo,
                ScanContext::Claude
            )
            .unwrap()
            .1,
        ScanFileTransition::Rescan
    );
}

#[test]
fn claude_latest_identity_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let newest = claude("2026-01-01T12:00:00Z", "same")
        .replace("\"effort\":\"high\"", "\"perTurnEffort\":\"HIGH\"");
    let older = claude("2026-01-01T11:00:00Z", "same");
    fs::write(&path, format!("{newest}\n{older}\n")).unwrap();
    let (events, _) = LocalUsageScanner::new()
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].timestamp.to_rfc3339(),
        "2026-01-01T12:00:00+00:00"
    );
    assert_eq!(events[0].effort.as_deref(), Some("high"));
    assert_eq!(events[0].cache_creation_1h_tokens, 4);
}

#[test]
fn pinned_scanner_reuses_slot_and_reports_backdated_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            claude("2026-01-01T12:00:00Z", "a"),
            claude("2026-01-02T12:00:00Z", "b")
        ),
    )
    .unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner.set_active_boundary(Some("2026-01-01"), "2026-01-03");
    assert_eq!(
        scanner
            .scan_active_boundary(&path, UTC, ScanContext::Claude)
            .unwrap()
            .0
            .len(),
        1
    );
    scanner.stats.bytes_read = 0;
    assert_eq!(
        scanner
            .scan_active_boundary(&path, UTC, ScanContext::Claude)
            .unwrap()
            .1,
        ScanFileTransition::Reuse
    );
    assert_eq!(scanner.stats.bytes_read, 0);
    writeln!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "{}",
        claude("2025-12-31T12:00:00Z", "old")
    )
    .unwrap();
    let (events, transition) = scanner
        .scan_active_boundary(&path, UTC, ScanContext::Claude)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(transition, ScanFileTransition::Resume { new_events, .. } if new_events.len() == 1 && new_events[0].message_id == "old")
    );
    scanner.set_active_boundary(Some("2026-01-02"), "2026-01-03");
    assert_eq!(
        scanner
            .scan_active_boundary(&path, UTC, ScanContext::Claude)
            .unwrap()
            .1,
        ScanFileTransition::Rescan
    );
    scanner.set_active_boundary(None, "2026-01-03");
    assert!(scanner
        .scan_active_boundary(&path, UTC, ScanContext::Claude)
        .is_err());
}

#[test]
fn antigravity_last_step_wins_and_wal_never_creates_shm() {
    use ccusage_core::scanner::antigravity::workspaces;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let row = |timestamp: &str, model: &str| {
        format!(
            r#"{{"step_index":1,"source":"MODEL","created_at":"{timestamp}","model":"{model}","input_tokens":2}}"#
        )
    };
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            row("2026-01-02T12:00:00Z", "gemini-high"),
            row("2026-01-02T11:00:00Z", "gemini-xhigh")
        ),
    )
    .unwrap();
    let (events, _) = LocalUsageScanner::new()
        .scan_file(
            &path,
            "2026-01-02",
            UTC,
            ScanContext::Antigravity {
                conversation: "id".into(),
                directory: Some("/work".into()),
            },
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].effort.as_deref(), Some("xhigh"));
    let db = dir.path().join("conversation_summaries.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("CREATE TABLE conversation_summaries(conversation_id TEXT, workspace_uris TEXT); INSERT INTO conversation_summaries VALUES ('id', '[\"file:///old\"]');").unwrap();
    drop(connection);
    fs::write(
        dir.path().join("conversation_summaries.db-wal"),
        b"not a valid wal",
    )
    .unwrap();
    assert!(workspaces(dir.path()).is_none());
    assert!(!dir.path().join("conversation_summaries.db-shm").exists());
}

#[test]
fn live_wal_workspace_and_historical_scan_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("conversation_summaries.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE conversation_summaries(conversation_id TEXT, workspace_uris TEXT); INSERT INTO conversation_summaries VALUES ('id', '[\"file:///live\"]');").unwrap();
    assert!(dir.path().join("conversation_summaries.db-wal").exists());
    assert_eq!(
        ccusage_core::scanner::antigravity::workspaces(dir.path())
            .unwrap()
            .get("id")
            .map(String::as_str),
        Some("/live")
    );
    let path = dir.path().join("a.jsonl");
    fs::write(&path, format!("{}\n", claude("2026-01-01T12:00:00Z", "a"))).unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner.set_active_boundary(Some("2026-01-01"), "2026-01-03");
    assert_eq!(
        scanner
            .scan_historical_once(&path, "2026-01-01", UTC, ScanContext::Claude)
            .unwrap()
            .0
            .len(),
        1
    );
    assert!(scanner.entries.is_empty());
    assert_eq!(
        scanner
            .scan_active_boundary(&path, UTC, ScanContext::Claude)
            .unwrap()
            .1,
        ScanFileTransition::Rescan
    );
    drop(connection);
}

#[test]
fn resume_reads_tail_and_boundary_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let initial = format!("{}\n", claude("2026-01-01T12:00:00Z", "a"));
    fs::write(&path, &initial).unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    let appended = format!("{}\n", claude("2026-01-01T13:00:00Z", "b"));
    write!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "{appended}"
    )
    .unwrap();
    scanner.stats.bytes_read = 0;
    let (events, transition) = scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(
        matches!(transition, ScanFileTransition::Resume { new_events, .. } if new_events.len() == 1)
    );
    assert_eq!(
        scanner.stats.bytes_read,
        initial.len().min(256) as u64
            + appended.len() as u64
            + (initial.len() + appended.len()).min(256) as u64
    );
}

#[test]
fn review_long_codex_metadata_reverse_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let mut file = fs::File::create(&path).unwrap();
    writeln!(file, "{{\"timestamp\":\"2025-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"session-real\",\"cwd\":\"/work/proj\",\"base_instructions\":\"{}\"}}}}", "x".repeat(11000)).unwrap();
    for _ in 0..2000 {
        writeln!(
            file,
            "{{\"timestamp\":\"2025-01-01T00:00:00Z\",\"type\":\"other\",\"payload\":{{}}}}"
        )
        .unwrap();
    }
    writeln!(file, "{{\"timestamp\":\"2026-01-02T00:00:00Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"m\",\"effort\":\"high\"}}}}").unwrap();
    let token = |n| {
        serde_json::json!({"timestamp":"2026-01-02T01:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1},"total_token_usage":{"input_tokens":n}}}}).to_string()
    };
    writeln!(file, "{}", token(1)).unwrap();
    drop(file);
    let context = || ScanContext::Codex(Default::default());
    let mut bounded = LocalUsageScanner::new();
    let (reverse, _) = bounded
        .scan_file(&path, "2026-01-02", UTC, context())
        .unwrap();
    let (forward, _) = LocalUsageScanner::new()
        .scan_file(&path, "", UTC, context())
        .unwrap();
    assert_eq!(reverse, forward);
    assert_eq!(reverse[0].session_id, "session-real");
    assert_eq!(reverse[0].directory.as_deref(), Some("/work/proj"));
    assert!(bounded.stats.bytes_read < fs::metadata(&path).unwrap().len());
    writeln!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "{}",
        token(2)
    )
    .unwrap();
    let (events, transition) = bounded
        .scan_file(&path, "2026-01-02", UTC, context())
        .unwrap();
    assert!(matches!(transition, ScanFileTransition::Resume { .. }));
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.session_id == "session-real"
        && e.directory.as_deref() == Some("/work/proj")
        && e.model == "m"
        && e.effort.as_deref() == Some("high")));
}

#[test]
fn review_closed_wal_and_invalid_workspace_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("conversation_summaries.db");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE conversation_summaries(conversation_id, workspace_uris); INSERT INTO conversation_summaries VALUES ('null', NULL), (NULL, '[]'), (123, '[]'), ('bad', 42), ('valid', '[\"file:///projects/a%20fixture\"]');").unwrap();
    drop(connection);
    let wal = dir.path().join("conversation_summaries.db-wal");
    let shm = dir.path().join("conversation_summaries.db-shm");
    assert!(!wal.exists() && !shm.exists());
    let map = ccusage_core::scanner::antigravity::workspaces(dir.path()).unwrap();
    assert_eq!(map.len(), 1);
    assert_eq!(
        map.get("valid").map(String::as_str),
        Some("/projects/a fixture")
    );
    assert!(!wal.exists() && !shm.exists());
}

#[test]
fn review_resume_preserves_cached_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            claude("2026-01-01T12:00:00Z", "a"),
            claude("2026-01-05T12:00:00Z", "b")
        ),
    )
    .unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    write!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "{}\n{}\n",
        claude("2026-01-03T12:00:00Z", "c"),
        claude("2026-01-05T13:00:00Z", "d")
    )
    .unwrap();
    let (narrow, transition) = scanner
        .scan_file(&path, "2026-01-05", UTC, ScanContext::Claude)
        .unwrap();
    assert!(
        matches!(transition, ScanFileTransition::Resume { new_events, .. } if new_events.len() == 2)
    );
    assert_eq!(narrow.len(), 2);
    assert_eq!(scanner.entries.values().next().unwrap().since, "2026-01-01");
    scanner.stats.bytes_read = 0;
    let (wide, transition) = scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    assert_eq!(transition, ScanFileTransition::Reuse);
    assert_eq!(wide.len(), 4);
    assert_eq!(scanner.stats.bytes_read, 0);
}

#[test]
fn review_prune_updates_coverage() {
    use ccusage_core::scanner::file_scan::{scan_step, ScanCacheRetention, ScanStep};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            claude("2026-01-05T12:00:00Z", "a"),
            claude("2026-01-10T12:00:00Z", "b")
        ),
    )
    .unwrap();
    let mut scanner = LocalUsageScanner::new();
    scanner
        .scan_file(&path, "2026-01-01", UTC, ScanContext::Claude)
        .unwrap();
    let path = path.canonicalize().unwrap();
    let mut retention = ScanCacheRetention::new("2026-01-01".into());
    retention.entries = scanner.entries;
    retention.prune("2026-01-10");
    let entry = retention.entries.get(&path).unwrap();
    assert_eq!(entry.since, "2026-01-10");
    assert_eq!(entry.events.len(), 1);
    assert_eq!(
        scan_step(&path, Some(entry), "2026-01-05", "UTC").unwrap(),
        ScanStep::Rescan
    );
    assert_eq!(
        scan_step(&path, Some(entry), "2026-01-10", "UTC").unwrap(),
        ScanStep::Reuse
    );
}
