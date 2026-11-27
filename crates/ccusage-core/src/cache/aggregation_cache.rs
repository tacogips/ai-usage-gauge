use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use rust_decimal::Decimal;
use thiserror::Error;

use super::lifecycle::{
    CacheLifecycleError, LocalCacheMigrator, MachineCacheRecovery, MigrationOutcome,
};
pub use crate::types::{AggregationCachePayload, AggregationCacheRange, LegacyPartitionDecision};
use crate::types::{MetricRecord, PricedUsageEvent, SessionMetricRecord, UsageDataQuality};

#[derive(Debug, Error)]
pub enum AggregationCacheError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Lifecycle(#[from] CacheLifecycleError),
    #[error("stale source generation")]
    StaleGeneration,
    #[error("invalid cache data: {0}")]
    InvalidData(String),
}

type Result<T> = std::result::Result<T, AggregationCacheError>;
type CacheMetadataRow = (f64, f64, Option<String>, String, Option<String>);
type SessionGroupKey = (
    DateTime<Utc>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);

fn unix_seconds_to_utc(timestamp: f64) -> Result<DateTime<Utc>> {
    if !timestamp.is_finite() {
        return Err(AggregationCacheError::InvalidData(
            "invalid timestamp".into(),
        ));
    }
    let floor = timestamp.floor();
    let mut seconds = floor as i64;
    let mut nanos = ((timestamp - floor) * 1_000_000_000.0).round() as u32;
    if nanos == 1_000_000_000 {
        seconds = seconds
            .checked_add(1)
            .ok_or_else(|| AggregationCacheError::InvalidData("invalid timestamp".into()))?;
        nanos = 0;
    }
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .ok_or_else(|| AggregationCacheError::InvalidData("invalid timestamp".into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageClassification {
    Uncovered,
    AggregateCoveredProvenanceUnscanned,
    LegacyCoveredNeedsAntigravity,
    NativeComplete,
}
/// Classify a raw partition without inferring attribution from an incomplete log.
pub fn evaluate_legacy_partition_completeness(
    cached: &[MetricRecord],
    scanned: &[SessionMetricRecord],
) -> LegacyPartitionDecision {
    if scanned.is_empty() {
        return LegacyPartitionDecision::Absent;
    }
    let mut expected = [0u128; 4];
    let mut actual = [0u128; 4];
    for row in cached {
        for (sum, value) in expected.iter_mut().zip([
            row.input_tokens,
            row.output_tokens,
            row.cache_creation_tokens,
            row.cache_read_tokens,
        ]) {
            *sum += u128::from(value);
        }
    }
    for row in scanned {
        for (sum, value) in actual.iter_mut().zip([
            row.input_tokens,
            row.output_tokens,
            row.cache_creation_tokens,
            row.cache_read_tokens,
        ]) {
            *sum += u128::from(value);
        }
    }
    let cached_cost = cached
        .iter()
        .try_fold(Decimal::ZERO, |sum, r| sum.checked_add(r.cost_usd));
    let raw_cost = scanned
        .iter()
        .try_fold(Decimal::ZERO, |sum, r| sum.checked_add(r.cost_usd));
    if !cached.is_empty()
        && expected == actual
        && (expected.iter().any(|n| *n > 0)
            || (cached_cost == Some(Decimal::ZERO) && raw_cost == Some(Decimal::ZERO)))
    {
        LegacyPartitionDecision::Complete
    } else {
        LegacyPartitionDecision::PartialOrUnverifiable
    }
}

/// Convert exact raw events into 15-minute inputs, preserving each event for
/// authoritative legacy cost allocation. `merge_day` subsequently groups them.
pub fn events_to_session_buckets(
    events: &[PricedUsageEvent],
    start: f64,
    end: f64,
    machine: &str,
) -> Result<Vec<SessionMetricRecord>> {
    let mut result = Vec::with_capacity(events.len());
    for priced in events {
        let event = &priced.event;
        let instant = event.timestamp.timestamp() as f64
            + f64::from(event.timestamp.timestamp_subsec_nanos()) / 1_000_000_000.0;
        let floor = event.timestamp.timestamp().div_euclid(900) * 900;
        if instant < start || instant >= end {
            return Err(AggregationCacheError::InvalidData(
                "event or floored bucket outside local day bounds".into(),
            ));
        }
        let timestamp = DateTime::<Utc>::from_timestamp(floor, 0)
            .ok_or_else(|| AggregationCacheError::InvalidData("invalid bucket timestamp".into()))?;
        let total_tokens = event
            .input_tokens
            .checked_add(event.output_tokens)
            .and_then(|v| v.checked_add(event.cache_creation_tokens))
            .and_then(|v| v.checked_add(event.cache_read_tokens))
            .ok_or_else(|| AggregationCacheError::InvalidData("token total overflow".into()))?;
        result.push(SessionMetricRecord {
            timestamp,
            agent: event.agent.clone(),
            model: event.model.clone(),
            cost_usd: priced.cost_usd,
            input_tokens: event.input_tokens,
            output_tokens: event.output_tokens,
            cache_creation_tokens: event.cache_creation_tokens,
            cache_read_tokens: event.cache_read_tokens,
            total_tokens,
            data_quality: UsageDataQuality::Timestamped,
            machine: machine.to_owned(),
            directory: event.directory.clone(),
            effort: event.effort.clone(),
        });
    }
    Ok(result)
}

pub struct UsageAggregationCache {
    connection: Connection,
    machine_id: String,
    expected_fingerprint: Option<String>,
    generation: u64,
}

impl UsageAggregationCache {
    pub fn open(path: impl AsRef<Path>, machine_id: impl Into<String>) -> Result<Self> {
        let path = path.as_ref();
        let machine_id = machine_id.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        MachineCacheRecovery::reconcile(&machine_id, path)?;
        if machine_id == "local"
            && path
                .file_name()
                .is_some_and(|name| name == "aggregates-local.sqlite3")
        {
            let legacy = path.with_file_name("aggregates.sqlite3");
            if LocalCacheMigrator::new(legacy, path).migrate_if_needed()?
                == MigrationOutcome::Conflict
            {
                eprintln!("legacy local cache migration skipped: destination already exists");
            }
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        let mut cache = Self {
            connection,
            machine_id,
            expected_fingerprint: None,
            generation: 0,
        };
        cache.create_schema()?;
        Ok(cache)
    }

    fn create_schema(&mut self) -> Result<()> {
        self.connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS cache_metadata(created_at REAL NOT NULL, updated_at REAL NOT NULL, cached_from TEXT, cached_through TEXT NOT NULL, source_configuration_fingerprint TEXT);
        CREATE TABLE IF NOT EXISTS daily_metrics(date TEXT NOT NULL, agent TEXT NOT NULL, model TEXT NOT NULL, cost_usd TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, cache_creation_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL, directory TEXT);
        CREATE TABLE IF NOT EXISTS session_metrics(timestamp REAL NOT NULL, agent TEXT NOT NULL, model TEXT NOT NULL, cost_usd TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, cache_creation_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL, data_quality TEXT NOT NULL, directory TEXT, effort TEXT);
        CREATE INDEX IF NOT EXISTS daily_metrics_date_idx ON daily_metrics(date);
        CREATE INDEX IF NOT EXISTS session_metrics_timestamp_idx ON session_metrics(timestamp);
        CREATE TABLE IF NOT EXISTS coverage_ranges(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day));
        CREATE TABLE IF NOT EXISTS pending_range_jobs(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day));
        CREATE TABLE IF NOT EXISTS directory_coverage_ranges(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day));")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (table, column, kind) in [
            ("cache_metadata", "cached_from", "TEXT"),
            ("cache_metadata", "source_configuration_fingerprint", "TEXT"),
            ("daily_metrics", "directory", "TEXT"),
            ("session_metrics", "directory", "TEXT"),
            ("session_metrics", "effort", "TEXT"),
        ] {
            let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if !names.iter().any(|name| name == column) {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
                if table == "session_metrics" && column == "effort" && self.machine_id == "local" {
                    tx.execute("DELETE FROM directory_coverage_ranges", [])?;
                }
            }
        }
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            if self.machine_id == "local" {
                tx.execute("DELETE FROM directory_coverage_ranges", [])?;
            }
            tx.pragma_update(None, "user_version", 1)?;
        }
        if self.machine_id == "local" {
            let mut stmt = tx.prepare("PRAGMA table_info(session_metrics)")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if !names.iter().any(|name| name == "source_day") {
                tx.execute_batch("ALTER TABLE session_metrics ADD COLUMN source_day TEXT")?;
            }
            tx.execute_batch("CREATE INDEX IF NOT EXISTS session_metrics_source_day_idx ON session_metrics(source_day)")?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS antigravity_coverage_ranges(since_day TEXT NOT NULL, through_day TEXT NOT NULL, PRIMARY KEY(since_day, through_day));")?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn begin_source_attempt(&mut self, fingerprint: &str) -> Result<u64> {
        let stored: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT source_configuration_fingerprint FROM cache_metadata LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match stored {
            Some(None) => {
                self.connection.execute("UPDATE cache_metadata SET source_configuration_fingerprint=?1 WHERE source_configuration_fingerprint IS NULL", [fingerprint])?;
            }
            Some(Some(existing)) if existing != fingerprint => {
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                for table in [
                    "cache_metadata",
                    "daily_metrics",
                    "session_metrics",
                    "coverage_ranges",
                    "directory_coverage_ranges",
                    "pending_range_jobs",
                ] {
                    tx.execute(&format!("DELETE FROM {table}"), [])?;
                }
                if self.machine_id == "local" {
                    tx.execute("DELETE FROM antigravity_coverage_ranges", [])?;
                }
                tx.commit()?;
            }
            _ => {}
        }
        if self.expected_fingerprint.as_deref() != Some(fingerprint) {
            self.generation = self.generation.wrapping_add(1);
            self.expected_fingerprint = Some(fingerprint.to_owned());
        }
        Ok(self.generation)
    }

    /// Read a Swift-compatible snapshot, returning none when disabled or expired.
    pub fn load(
        &mut self,
        now: f64,
        retention_days: i64,
        fingerprint: Option<&str>,
    ) -> Result<Option<AggregationCachePayload>> {
        if retention_days <= 0 {
            return Ok(None);
        }
        if let Some(fingerprint) = fingerprint {
            self.begin_source_attempt(fingerprint)?;
        }
        let metadata: Option<CacheMetadataRow> = self.connection.query_row(
            "SELECT created_at,updated_at,cached_from,cached_through,source_configuration_fingerprint FROM cache_metadata LIMIT 1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
        ).optional()?;
        let Some((
            created_at,
            updated_at,
            cached_from,
            cached_through,
            source_configuration_fingerprint,
        )) = metadata
        else {
            return Ok(None);
        };
        if now - created_at >= retention_days as f64 * 86_400.0 {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            for table in [
                "cache_metadata",
                "daily_metrics",
                "session_metrics",
                "coverage_ranges",
                "directory_coverage_ranges",
                "pending_range_jobs",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            if self.machine_id == "local" {
                tx.execute("DELETE FROM antigravity_coverage_ranges", [])?;
            }
            tx.commit()?;
            return Ok(None);
        }
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT date FROM daily_metrics ORDER BY date")?;
        let days = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut metrics = Vec::new();
        for day in &days {
            metrics.extend(read_daily(&self.connection, day, &self.machine_id)?);
        }
        let sessions = read_sessions(&self.connection, f64::MIN, f64::MAX, &self.machine_id)?;
        let fallback_from = cached_from.clone();
        let cached_from = cached_from.unwrap_or_else(|| {
            days.first()
                .cloned()
                .unwrap_or_else(|| cached_through.clone())
        });
        let mut covered_ranges = read_ranges(&self.connection, "coverage_ranges")?;
        if covered_ranges.is_empty() {
            if let Some(since) = fallback_from {
                covered_ranges.push(AggregationCacheRange {
                    since,
                    through: cached_through.clone(),
                });
            }
        }
        let directory_covered_ranges = read_ranges(&self.connection, "directory_coverage_ranges")?;
        let antigravity_covered_ranges = if self.machine_id == "local" {
            read_ranges(&self.connection, "antigravity_coverage_ranges")?
        } else {
            Vec::new()
        };
        Ok(Some(AggregationCachePayload {
            created_at: unix_seconds_to_utc(created_at)?,
            updated_at: unix_seconds_to_utc(updated_at)?,
            cached_from,
            cached_through,
            metrics,
            sessions,
            covered_ranges,
            directory_covered_ranges,
            antigravity_covered_ranges,
            source_configuration_fingerprint,
        }))
    }

    pub fn classify_day(&self, day: &str) -> Result<CoverageClassification> {
        classify(&self.connection, &self.machine_id, day)
    }
    pub fn begin_job(&self, since: &str, through: &str) -> Result<()> {
        if since <= through {
            self.connection.execute(
                "INSERT OR IGNORE INTO pending_range_jobs VALUES (?1, ?2)",
                params![since, through],
            )?;
        }
        Ok(())
    }
    pub fn finish_job(&self, since: &str, through: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM pending_range_jobs WHERE since_day=?1 AND through_day=?2",
            params![since, through],
        )?;
        Ok(())
    }
    pub fn pending_jobs(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT since_day, through_day FROM pending_range_jobs ORDER BY since_day, through_day",
        )?;
        let jobs = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(jobs)
    }

    /// Commit all changes for one local calendar day. Bounds are Unix seconds in the caller's calendar.
    #[allow(clippy::too_many_arguments)]
    pub fn merge_day(
        &mut self,
        day: &str,
        start: f64,
        end: f64,
        daily: &[MetricRecord],
        sessions: &[SessionMetricRecord],
        fingerprint: &str,
        generation: u64,
    ) -> Result<()> {
        if self.expected_fingerprint.as_deref() != Some(fingerprint)
            || self.generation != generation
        {
            return Err(AggregationCacheError::StaleGeneration);
        }
        if start >= end
            || daily.iter().any(|r| r.date != day)
            || sessions.iter().any(|r| {
                let t = r.timestamp.timestamp_millis() as f64 / 1000.0;
                t < if self.machine_id == "local" {
                    ((start.floor() as i64).div_euclid(900) * 900) as f64
                } else {
                    start
                } || t >= end
            })
        {
            return Err(AggregationCacheError::InvalidData(
                "day rows outside supplied bounds".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT OR IGNORE INTO coverage_ranges(since_day,through_day) SELECT cached_from,cached_through FROM cache_metadata WHERE cached_from IS NOT NULL AND NOT EXISTS(SELECT 1 FROM coverage_ranges)", [])?;
        let state = classify(&tx, &self.machine_id, day)?;
        if self.machine_id != "local" {
            if state == CoverageClassification::Uncovered {
                tx.execute("DELETE FROM daily_metrics WHERE date=?1", [day])?;
                for row in daily {
                    insert_daily(&tx, row)?;
                }
                mark(&tx, "coverage_ranges", day)?;
            }
            tx.execute(
                "DELETE FROM session_metrics WHERE timestamp>=?1 AND timestamp<?2",
                params![start, end],
            )?;
            for row in group_sessions(sessions)? {
                insert_session(&tx, &row, None)?;
            }
            mark(&tx, "directory_coverage_ranges", day)?;
        } else {
            match state {
                CoverageClassification::Uncovered => {
                    tx.execute("DELETE FROM daily_metrics WHERE date=?1", [day])?;
                    tx.execute(
                        "DELETE FROM session_metrics WHERE source_day=?1 OR (source_day IS NULL AND timestamp>=?2 AND timestamp<?3)",
                        params![day, start, end],
                    )?;
                    for row in daily {
                        insert_daily(&tx, row)?;
                    }
                    for row in group_sessions(sessions)? {
                        insert_session(&tx, &row, Some(day))?;
                    }
                    mark(&tx, "coverage_ranges", day)?;
                    mark(&tx, "directory_coverage_ranges", day)?;
                    mark(&tx, "antigravity_coverage_ranges", day)?;
                }
                CoverageClassification::AggregateCoveredProvenanceUnscanned => {
                    let cached = read_daily(&tx, day, "local")?;
                    let mut partitions: BTreeMap<(String, String), Vec<MetricRecord>> =
                        BTreeMap::new();
                    for row in cached
                        .into_iter()
                        .filter(|r| r.agent == "claude" || r.agent == "codex")
                    {
                        partitions
                            .entry((row.agent.clone(), row.model.clone()))
                            .or_default()
                            .push(row);
                    }
                    for ((agent, model), rows) in partitions {
                        let raw: Vec<_> = sessions
                            .iter()
                            .filter(|r| r.agent == agent && r.model == model)
                            .cloned()
                            .collect();
                        if evaluate_legacy_partition_completeness(&rows, &raw)
                            == LegacyPartitionDecision::Complete
                        {
                            tx.execute("DELETE FROM session_metrics WHERE (source_day=?1 OR (source_day IS NULL AND timestamp>=?2 AND timestamp<?3)) AND agent=?4 AND model=?5", params![day,start,end,agent,model])?;
                            let cost = rows
                                .iter()
                                .try_fold(Decimal::ZERO, |sum, r| sum.checked_add(r.cost_usd))
                                .ok_or_else(|| {
                                    AggregationCacheError::InvalidData(
                                        "cached cost overflow".into(),
                                    )
                                })?;
                            let raw_cost = raw
                                .iter()
                                .try_fold(Decimal::ZERO, |sum, r| sum.checked_add(r.cost_usd))
                                .ok_or_else(|| {
                                    AggregationCacheError::InvalidData("raw cost overflow".into())
                                })?;
                            let mut allocated = Decimal::ZERO;
                            let mut allocated_rows = Vec::with_capacity(raw.len());
                            for (index, mut row) in raw.iter().cloned().enumerate() {
                                row.cost_usd = if index + 1 == raw.len() {
                                    cost.checked_sub(allocated).ok_or_else(|| {
                                        AggregationCacheError::InvalidData(
                                            "cost allocation overflow".into(),
                                        )
                                    })?
                                } else if raw_cost.is_zero() {
                                    Decimal::ZERO
                                } else {
                                    cost.checked_mul(row.cost_usd)
                                        .and_then(|v| v.checked_div(raw_cost))
                                        .ok_or_else(|| {
                                            AggregationCacheError::InvalidData(
                                                "cost allocation overflow".into(),
                                            )
                                        })?
                                };
                                allocated =
                                    allocated.checked_add(row.cost_usd).ok_or_else(|| {
                                        AggregationCacheError::InvalidData(
                                            "cost allocation overflow".into(),
                                        )
                                    })?;
                                allocated_rows.push(row);
                            }
                            for row in group_sessions(&allocated_rows)? {
                                insert_session(&tx, &row, Some(day))?;
                            }
                        }
                    }
                    replace_agy(&tx, day, start, end, daily, sessions)?;
                    mark(&tx, "directory_coverage_ranges", day)?;
                    mark(&tx, "antigravity_coverage_ranges", day)?;
                }
                CoverageClassification::LegacyCoveredNeedsAntigravity => {
                    replace_agy(&tx, day, start, end, daily, sessions)?;
                    mark(&tx, "antigravity_coverage_ranges", day)?;
                }
                CoverageClassification::NativeComplete => {}
            }
        }
        let now = Utc::now().timestamp_millis() as f64 / 1000.0;
        let exists: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM cache_metadata)", [], |r| {
                r.get(0)
            })?;
        if exists {
            tx.execute("UPDATE cache_metadata SET updated_at=?1, cached_from=CASE WHEN cached_from IS NULL OR cached_from>?2 THEN ?2 ELSE cached_from END, cached_through=MAX(cached_through, ?2), source_configuration_fingerprint=?3", params![now,day,fingerprint])?;
        } else {
            tx.execute(
                "INSERT INTO cache_metadata(created_at,updated_at,cached_from,cached_through,source_configuration_fingerprint) VALUES (?1,?1,?2,?2,?3)",
                params![now, day, fingerprint],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn daily_metrics(&self, day: &str) -> Result<Vec<MetricRecord>> {
        read_daily(&self.connection, day, &self.machine_id)
    }
    pub fn session_metrics(&self, start: f64, end: f64) -> Result<Vec<SessionMetricRecord>> {
        read_sessions(&self.connection, start, end, &self.machine_id)
    }
    pub fn session_metrics_for_day(
        &self,
        day: &str,
        start: f64,
        end: f64,
    ) -> Result<Vec<SessionMetricRecord>> {
        if self.machine_id == "local" {
            read_sessions_query(
                &self.connection,
                "source_day=?1 OR (source_day IS NULL AND timestamp>=?2 AND timestamp<?3)",
                params![day, start, end],
                &self.machine_id,
            )
        } else {
            read_sessions(&self.connection, start, end, &self.machine_id)
        }
    }
}

fn read_ranges(db: &Connection, table: &str) -> Result<Vec<AggregationCacheRange>> {
    let mut statement = db.prepare(&format!(
        "SELECT since_day,through_day FROM {table} ORDER BY since_day,through_day"
    ))?;
    let ranges = statement
        .query_map([], |r| {
            Ok(AggregationCacheRange {
                since: r.get(0)?,
                through: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut normalized: Vec<AggregationCacheRange> = Vec::new();
    for range in ranges {
        if let Some(last) = normalized.last_mut() {
            let adjacent = chrono::NaiveDate::parse_from_str(&last.through, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.succ_opt())
                .map(|d| d.format("%Y-%m-%d").to_string());
            if range.since <= last.through || adjacent.as_deref() == Some(range.since.as_str()) {
                if range.through > last.through {
                    last.through = range.through;
                }
                continue;
            }
        }
        normalized.push(range);
    }
    Ok(normalized)
}
fn covered(db: &Connection, table: &str, day: &str) -> Result<bool> {
    let matched: bool = db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE since_day<=?1 AND through_day>=?1)"),
        [day],
        |r| r.get(0),
    )?;
    if matched || table != "coverage_ranges" {
        return Ok(matched);
    }
    Ok(db.query_row("SELECT NOT EXISTS(SELECT 1 FROM coverage_ranges) AND EXISTS(SELECT 1 FROM cache_metadata WHERE cached_from IS NOT NULL AND cached_from<=?1 AND cached_through>=?1)", [day], |r| r.get(0))?)
}
fn classify(db: &Connection, machine: &str, day: &str) -> Result<CoverageClassification> {
    if !covered(db, "coverage_ranges", day)? {
        return Ok(CoverageClassification::Uncovered);
    }
    if !covered(db, "directory_coverage_ranges", day)? {
        return Ok(CoverageClassification::AggregateCoveredProvenanceUnscanned);
    }
    if machine != "local" || covered(db, "antigravity_coverage_ranges", day)? {
        Ok(CoverageClassification::NativeComplete)
    } else {
        Ok(CoverageClassification::LegacyCoveredNeedsAntigravity)
    }
}
fn mark(tx: &Transaction<'_>, table: &str, day: &str) -> Result<()> {
    tx.execute(
        &format!("INSERT OR IGNORE INTO {table}(since_day,through_day) VALUES (?1,?1)"),
        [day],
    )?;
    Ok(())
}
fn insert_daily(tx: &Transaction<'_>, row: &MetricRecord) -> Result<()> {
    tx.execute("INSERT INTO daily_metrics(date,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,directory) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![row.date,row.agent,row.model,row.cost_usd.to_string(),row.input_tokens,row.output_tokens,row.cache_creation_tokens,row.cache_read_tokens,row.directory])?;
    Ok(())
}
fn group_sessions(rows: &[SessionMetricRecord]) -> Result<Vec<SessionMetricRecord>> {
    let mut grouped: BTreeMap<SessionGroupKey, SessionMetricRecord> = BTreeMap::new();
    for row in rows {
        let quality = match row.data_quality {
            UsageDataQuality::Timestamped => "timestamped",
            UsageDataQuality::SessionEstimated => "sessionEstimated",
        };
        let key = (
            row.timestamp,
            row.agent.clone(),
            row.model.clone(),
            row.directory.clone(),
            row.effort.clone(),
            quality.to_owned(),
        );
        if let Some(current) = grouped.get_mut(&key) {
            current.cost_usd = current
                .cost_usd
                .checked_add(row.cost_usd)
                .ok_or_else(|| AggregationCacheError::InvalidData("bucket cost overflow".into()))?;
            current.input_tokens = current
                .input_tokens
                .checked_add(row.input_tokens)
                .ok_or_else(|| {
                    AggregationCacheError::InvalidData("bucket token overflow".into())
                })?;
            current.output_tokens = current
                .output_tokens
                .checked_add(row.output_tokens)
                .ok_or_else(|| {
                    AggregationCacheError::InvalidData("bucket token overflow".into())
                })?;
            current.cache_creation_tokens = current
                .cache_creation_tokens
                .checked_add(row.cache_creation_tokens)
                .ok_or_else(|| {
                    AggregationCacheError::InvalidData("bucket token overflow".into())
                })?;
            current.cache_read_tokens = current
                .cache_read_tokens
                .checked_add(row.cache_read_tokens)
                .ok_or_else(|| {
                    AggregationCacheError::InvalidData("bucket token overflow".into())
                })?;
            current.total_tokens = current
                .input_tokens
                .checked_add(current.output_tokens)
                .and_then(|v| v.checked_add(current.cache_creation_tokens))
                .and_then(|v| v.checked_add(current.cache_read_tokens))
                .ok_or_else(|| {
                    AggregationCacheError::InvalidData("bucket token overflow".into())
                })?;
        } else {
            grouped.insert(key, row.clone());
        }
    }
    Ok(grouped.into_values().collect())
}
fn insert_session(
    tx: &Transaction<'_>,
    row: &SessionMetricRecord,
    day: Option<&str>,
) -> Result<()> {
    let quality = match row.data_quality {
        UsageDataQuality::Timestamped => "timestamped",
        UsageDataQuality::SessionEstimated => "sessionEstimated",
    };
    let columns = if day.is_some() { ",source_day" } else { "" };
    let value = if day.is_some() { ",?12" } else { "" };
    let sql = format!("INSERT INTO session_metrics(timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort{columns}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11{value})");
    let args = params![
        row.timestamp.timestamp_millis() as f64 / 1000.0,
        row.agent,
        row.model,
        row.cost_usd.to_string(),
        row.input_tokens,
        row.output_tokens,
        row.cache_creation_tokens,
        row.cache_read_tokens,
        quality,
        row.directory,
        row.effort
    ];
    if let Some(day) = day {
        tx.execute(
            &sql,
            rusqlite::params_from_iter(
                args.iter()
                    .copied()
                    .chain(std::iter::once(&day as &dyn rusqlite::ToSql)),
            ),
        )?;
    } else {
        tx.execute(&sql, args)?;
    }
    Ok(())
}
fn replace_agy(
    tx: &Transaction<'_>,
    day: &str,
    start: f64,
    end: f64,
    daily: &[MetricRecord],
    sessions: &[SessionMetricRecord],
) -> Result<()> {
    tx.execute(
        "DELETE FROM daily_metrics WHERE date=?1 AND agent='agy'",
        [day],
    )?;
    tx.execute(
        "DELETE FROM session_metrics WHERE (source_day=?1 OR (source_day IS NULL AND timestamp>=?2 AND timestamp<?3)) AND agent='agy'",
        params![day, start, end],
    )?;
    for row in daily.iter().filter(|r| r.agent == "agy") {
        insert_daily(tx, row)?;
    }
    let rows: Vec<_> = sessions
        .iter()
        .filter(|r| r.agent == "agy")
        .cloned()
        .collect();
    for row in group_sessions(&rows)? {
        insert_session(tx, &row, Some(day))?;
    }
    Ok(())
}
fn decimal_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Decimal> {
    let text: String = row.get(index)?;
    Decimal::from_str(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}
fn token_total(values: [u64; 4]) -> rusqlite::Result<u64> {
    values
        .into_iter()
        .try_fold(0u64, |sum, value| sum.checked_add(value))
        .ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))
}
fn read_daily(db: &Connection, day: &str, machine: &str) -> Result<Vec<MetricRecord>> {
    let mut statement = db.prepare("SELECT date,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,directory FROM daily_metrics WHERE date=?1 ORDER BY date,agent,model,directory")?;
    let rows = statement.query_map([day], |r| {
        let values = [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?];
        Ok(MetricRecord {
            date: r.get(0)?,
            agent: r.get(1)?,
            model: r.get(2)?,
            cost_usd: decimal_column(r, 3)?,
            input_tokens: values[0],
            output_tokens: values[1],
            cache_creation_tokens: values[2],
            cache_read_tokens: values[3],
            total_tokens: token_total(values)?,
            machine: machine.to_owned(),
            directory: r.get(8)?,
            effort: None,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
fn read_sessions(
    db: &Connection,
    start: f64,
    end: f64,
    machine: &str,
) -> Result<Vec<SessionMetricRecord>> {
    read_sessions_query(
        db,
        "timestamp>=?1 AND timestamp<?2",
        params![start, end],
        machine,
    )
}
fn read_sessions_query(
    db: &Connection,
    predicate: &str,
    params: impl rusqlite::Params,
    machine: &str,
) -> Result<Vec<SessionMetricRecord>> {
    let mut statement = db.prepare(&format!("SELECT timestamp,agent,model,cost_usd,input_tokens,output_tokens,cache_creation_tokens,cache_read_tokens,data_quality,directory,effort FROM session_metrics WHERE {predicate} ORDER BY timestamp,agent,model,directory,effort"))?;
    let rows = statement.query_map(params, |r| {
        let timestamp: f64 = r.get(0)?;
        let seconds = timestamp.floor() as i64;
        let nanos = ((timestamp - seconds as f64) * 1_000_000_000.0).round() as u32;
        let timestamp = DateTime::<Utc>::from_timestamp(seconds, nanos).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Real,
                "invalid timestamp".into(),
            )
        })?;
        let quality: String = r.get(8)?;
        let data_quality = match quality.as_str() {
            "timestamped" => UsageDataQuality::Timestamped,
            "sessionEstimated" => UsageDataQuality::SessionEstimated,
            _ => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    "invalid data quality".into(),
                ))
            }
        };
        let values = [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?];
        Ok(SessionMetricRecord {
            timestamp,
            agent: r.get(1)?,
            model: r.get(2)?,
            cost_usd: decimal_column(r, 3)?,
            input_tokens: values[0],
            output_tokens: values[1],
            cache_creation_tokens: values[2],
            cache_read_tokens: values[3],
            total_tokens: token_total(values)?,
            data_quality,
            machine: machine.to_owned(),
            directory: r.get(9)?,
            effort: r.get(10)?,
        })
    })?;
    group_sessions(&rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
