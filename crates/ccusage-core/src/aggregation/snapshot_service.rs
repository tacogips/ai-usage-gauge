use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;

use crate::aggregation::blocks::{local_midnight_utc, BlockBuilder, BlockContributor};
use crate::aggregation::buckets::aggregate_events;
use crate::aggregation::rolling::{days_between, selected_period_cost_ref, RollingInputsRef};
use crate::cache::{events_to_session_buckets, CoverageClassification, UsageAggregationCache};
use crate::pricing::PricingEngine;
use crate::scanner::{antigravity, codex::CodexScanContext, LocalUsageScanner, ScanContext};
use crate::state::ResetWindowCalculator;
use crate::types::{
    AppConfiguration, AppState, BudgetSummary, CostSnapshot, MachineDescriptor,
    MachineSessionSourcePlan, MetricRecord, PricedUsageEvent, ResetCycle, ScanFileTransition,
    ScanStats, SessionMetricRecord, TimestampedUsageEvent,
};

const RETENTION_LOOKBACK_DAYS: i64 = 8;

#[derive(Debug, Clone)]
struct DiscoveredFile {
    path: PathBuf,
    context: ScanContext,
}

pub struct SnapshotService {
    pub scanner: LocalUsageScanner,
    pub pricing: PricingEngine,
    pub block_builder: BlockBuilder,
    pub cache_path: PathBuf,
    pub antigravity_root: Option<PathBuf>,
    pub env: HashMap<String, String>,
    cache: Option<UsageAggregationCache>,
}

impl SnapshotService {
    pub fn new(
        cache_path: PathBuf,
        pricing: PricingEngine,
        antigravity_root: Option<PathBuf>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            scanner: LocalUsageScanner::new(),
            pricing,
            block_builder: BlockBuilder::new(),
            cache_path,
            antigravity_root,
            env,
            cache: None,
        }
    }

    pub fn scan_stats(&self) -> ScanStats {
        self.scanner.stats.clone()
    }

    pub fn reset_scan_stats(&mut self) {
        self.scanner.stats = ScanStats::default();
    }

    fn ensure_cache(&mut self) -> Result<&mut UsageAggregationCache, String> {
        if self.cache.is_none() {
            let opened = UsageAggregationCache::open(&self.cache_path, "local")
                .map_err(|e| format!("failed to open local aggregation cache: {e}"))?;
            self.cache = Some(opened);
        }
        Ok(self.cache.as_mut().expect("initialized"))
    }

    /// Refresh local usage natively in Rust with zero local `ccusage` CLI invocations.
    pub fn refresh_local(
        &mut self,
        descriptor: &MachineDescriptor,
        state: &AppState,
        config: &AppConfiguration,
        now: DateTime<Utc>,
        tz: Tz,
        desired_from: Option<NaiveDate>,
    ) -> Result<CostSnapshot, String> {
        let plan = MachineSessionSourcePlan::resolve_local(descriptor, &self.env);
        let fingerprint = plan.fingerprint().to_owned();

        let calc = ResetWindowCalculator::new(tz);
        let interval = calc
            .aggregation_interval(state.reset_cycle.clone(), now)
            .map_err(|e| e.to_string())?;
        let today = now.with_timezone(&tz).date_naive();
        let today_str = today.format("%Y-%m-%d").to_string();
        let retention_floor = today - Duration::days(RETENTION_LOOKBACK_DAYS);
        let floor_str = retention_floor.format("%Y-%m-%d").to_string();

        let interval_start_day = interval.0.with_timezone(&tz).date_naive();
        let initial_from = interval_start_day.min(today - Duration::days(1));
        let points_from = desired_from.map_or(initial_from, |d| d.min(initial_from));

        // Pin active boundary slot only for long CustomHours(n) windows starting before retention_floor.
        let boundary_day_opt = if matches!(state.reset_cycle, ResetCycle::CustomHours(_))
            && interval_start_day < retention_floor
        {
            Some(interval_start_day.format("%Y-%m-%d").to_string())
        } else {
            None
        };
        self.scanner
            .set_active_boundary(boundary_day_opt.as_deref(), &floor_str);

        // Discover all local log files across Claude, Codex, and Antigravity.
        let discovered = discover_local_files(&plan, self.antigravity_root.as_deref());

        // 1. Scan recent [retention_floor, today] incrementally.
        let mut recent_by_id: BTreeMap<String, TimestampedUsageEvent> = BTreeMap::new();
        let mut transitions: BTreeMap<PathBuf, ScanFileTransition> = BTreeMap::new();
        let mut contributors: BTreeMap<PathBuf, BlockContributor> = BTreeMap::new();

        for item in &discovered {
            let Ok((events, transition)) =
                self.scanner
                    .scan_file(&item.path, &floor_str, tz, item.context.clone())
            else {
                continue;
            };
            if let Some(entry) = self.scanner.entries.get(&item.path) {
                contributors.insert(
                    item.path.clone(),
                    BlockContributor {
                        canonical_path: item.path.clone(),
                        file_size: entry.file_size,
                        modified: entry.modified,
                        scanned_through: entry.scanned_through,
                        boundary: entry.boundary.clone(),
                    },
                );
            }
            transitions.insert(item.path.clone(), transition);
            for ev in events {
                insert_dedup(&mut recent_by_id, ev);
            }
        }

        let recent_priced: Vec<PricedUsageEvent> = recent_by_id
            .values()
            .map(|ev| self.pricing.price_event(ev))
            .collect();

        // 2. If long CustomHours(n) pinned a boundary day < retention_floor, scan it via pinned slot.
        let mut boundary_priced: Vec<PricedUsageEvent> = Vec::new();
        if boundary_day_opt.is_some() {
            let mut b_map: BTreeMap<String, TimestampedUsageEvent> = BTreeMap::new();
            for item in &discovered {
                if let Ok((events, _)) =
                    self.scanner
                        .scan_active_boundary(&item.path, tz, item.context.clone())
                {
                    for ev in events {
                        insert_dedup(&mut b_map, ev);
                    }
                }
            }
            boundary_priced = b_map
                .values()
                .map(|ev| self.pricing.price_event(ev))
                .collect();
        }

        // 3. Open SQLite cache, fence generation, and backfill any uncovered/legacy completed days.
        let now_secs = now.timestamp_millis() as f64 / 1000.0;
        let retention_days = i64::from(config.cache_retention_days);
        let cache = self.ensure_cache()?;
        let generation = cache
            .begin_source_attempt(&fingerprint)
            .map_err(|e| format!("cache begin_source_attempt failed: {e}"))?;
        let pre_load = cache
            .load(now_secs, retention_days, Some(&fingerprint))
            .map_err(|e| format!("cache load failed: {e}"))?;

        let mut recent_by_day: BTreeMap<NaiveDate, Vec<PricedUsageEvent>> = BTreeMap::new();
        for p in &recent_priced {
            let d = p.event.timestamp.with_timezone(&tz).date_naive();
            recent_by_day.entry(d).or_default().push(p.clone());
        }

        let mut legacy_local_days: BTreeSet<String> = BTreeSet::new();
        let mut wrote_backfill = false;
        let yesterday = today.pred_opt().unwrap_or(today);
        let backfill_start = points_from.min(retention_floor);
        if backfill_start <= yesterday {
            let mut uncovered_days: Vec<(NaiveDate, String)> = Vec::new();
            for day in days_between(backfill_start, yesterday) {
                let day_str = day.format("%Y-%m-%d").to_string();
                let class = self
                    .cache
                    .as_ref()
                    .expect("opened")
                    .classify_day(&day_str)
                    .map_err(|e| format!("classify_day failed: {e}"))?;
                if matches!(
                    class,
                    CoverageClassification::AggregateCoveredProvenanceUnscanned
                        | CoverageClassification::LegacyCoveredNeedsAntigravity
                ) {
                    legacy_local_days.insert(day_str.clone());
                }
                if class != CoverageClassification::NativeComplete {
                    uncovered_days.push((day, day_str));
                }
            }

            // Batch all uncovered days older than retention_floor into a single historical scan pass.
            let mut historical_by_day: BTreeMap<NaiveDate, Vec<PricedUsageEvent>> = BTreeMap::new();
            let old_days: Vec<NaiveDate> = uncovered_days
                .iter()
                .map(|(d, _)| *d)
                .filter(|d| *d < retention_floor)
                .collect();
            if let (Some(&min_old), Some(&max_old)) = (old_days.first(), old_days.last()) {
                let hist_events = scan_historical_range(
                    &mut self.scanner,
                    &self.pricing,
                    &discovered,
                    min_old,
                    max_old,
                    tz,
                );
                for ev in hist_events {
                    let d = ev.event.timestamp.with_timezone(&tz).date_naive();
                    historical_by_day.entry(d).or_default().push(ev);
                }
            }

            for (day, day_str) in uncovered_days {
                let day_events: &[PricedUsageEvent] = if day >= retention_floor {
                    recent_by_day.get(&day).map_or(&[], Vec::as_slice)
                } else {
                    historical_by_day.get(&day).map_or(&[], Vec::as_slice)
                };
                let (day_start, day_end) = local_day_bounds(day, tz);
                let start_s = day_start.timestamp_millis() as f64 / 1000.0;
                let end_s = day_end.timestamp_millis() as f64 / 1000.0;
                let (daily_rows, _) = aggregate_events(day_events, tz)
                    .map_err(|e| format!("aggregate_events failed: {e}"))?;
                let session_inputs =
                    events_to_session_buckets(day_events, start_s, end_s, "local")
                        .map_err(|e| format!("events_to_session_buckets failed: {e}"))?;
                self.cache
                    .as_mut()
                    .expect("opened")
                    .merge_day(
                        &day_str,
                        start_s,
                        end_s,
                        &daily_rows,
                        &session_inputs,
                        &fingerprint,
                        generation,
                    )
                    .map_err(|e| format!("merge_day failed: {e}"))?;
                wrote_backfill = true;
            }
        }

        // Also detect any pre-existing legacy days already marked covered in pre_load.
        if let Some(payload) = &pre_load {
            for r in &payload.covered_ranges {
                if !payload
                    .antigravity_covered_ranges
                    .iter()
                    .any(|a| a.since <= r.since && a.through >= r.through)
                {
                    if let (Ok(s), Ok(e)) = (
                        NaiveDate::parse_from_str(&r.since, "%Y-%m-%d"),
                        NaiveDate::parse_from_str(&r.through, "%Y-%m-%d"),
                    ) {
                        for d in days_between(s, e) {
                            legacy_local_days.insert(d.format("%Y-%m-%d").to_string());
                        }
                    }
                }
            }
        }

        // 4. Load merged historical daily & session metrics from SQLite + live today aggregates.
        // Reuse pre_load directly when no backfill writes occurred.
        let loaded = if wrote_backfill {
            self.cache
                .as_mut()
                .expect("opened")
                .load(now_secs, retention_days, Some(&fingerprint))
                .map_err(|e| format!("cache reload failed: {e}"))?
        } else {
            pre_load
        };

        let today_events: &[PricedUsageEvent] =
            recent_by_day.get(&today).map_or(&[], Vec::as_slice);
        let (today_daily, today_sessions) =
            aggregate_events(today_events, tz).map_err(|e| e.to_owned())?;

        let mut all_daily: Vec<MetricRecord> = loaded
            .as_ref()
            .map(|p| {
                p.metrics
                    .iter()
                    .filter(|m| m.date != today_str)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        all_daily.extend(today_daily);
        all_daily.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then(a.agent.cmp(&b.agent))
                .then(a.model.cmp(&b.model))
        });

        let (today_start, _) = local_day_bounds(today, tz);
        let mut all_sessions: Vec<SessionMetricRecord> = loaded
            .as_ref()
            .map(|p| {
                p.sessions
                    .iter()
                    .filter(|s| s.timestamp < today_start)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        all_sessions.extend(today_sessions);
        all_sessions.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then(a.agent.cmp(&b.agent))
                .then(a.model.cmp(&b.model))
        });

        // 5. Build 5-hour billing blocks via append-preserving BlockAnchor.
        let scanner_ref = &mut self.scanner;
        let pricing_ref = &self.pricing;
        let (points, _rebuilt) = self.block_builder.build_with_anchor(
            points_from,
            retention_floor,
            today,
            tz,
            &recent_priced,
            &contributors,
            &transitions,
            |since_d, through_d| -> Result<Vec<PricedUsageEvent>, String> {
                Ok(scan_historical_range(
                    scanner_ref,
                    pricing_ref,
                    &discovered,
                    since_d,
                    through_d,
                    tz,
                ))
            },
        )?;

        // 6. Compute exact rolling cost using shared selected_period_cost_ref (borrowing slices).
        let mut rolling_events = recent_priced;
        rolling_events.extend(boundary_priced);
        let rolling_inputs = RollingInputsRef {
            local_raw_events: &rolling_events,
            daily_metrics: &all_daily,
            session_metrics: &all_sessions,
            legacy_local_days: &legacy_local_days,
            today: &today_str,
            retention_floor: &floor_str,
        };
        let cost_since_reset =
            selected_period_cost_ref(&state.reset_cycle, interval, &rolling_inputs, &[], tz)
                .map_err(|e| e.to_owned())?;
        let budget = BudgetSummary::new(cost_since_reset, state.budget_usd);

        Ok(CostSnapshot {
            generated_at: now,
            active_boundary_at: interval.0,
            cost_since_reset_usd: cost_since_reset,
            budget,
            reset_cycle: state.reset_cycle.clone(),
            refresh_interval_seconds: state
                .refresh_interval_seconds
                .unwrap_or(config.poll_interval_seconds),
            points,
            dashboard_metrics: all_daily,
            dashboard_sessions: all_sessions,
            rolling_events,
        })
    }
}

impl CostSnapshot {
    /// Re-apply `AppState` (cycle, budget, refresh interval) onto an existing snapshot using the
    /// same `selected_period_cost` logic as tray, CLI, and `/api/budget`.
    pub fn applying(&self, state: &AppState, now: DateTime<Utc>, tz: Tz) -> Self {
        let calc = ResetWindowCalculator::new(tz);
        let interval = calc
            .aggregation_interval(state.reset_cycle.clone(), now)
            .unwrap_or((self.active_boundary_at, now));
        let today = now.with_timezone(&tz).date_naive();
        let floor = today - Duration::days(RETENTION_LOOKBACK_DAYS);

        let today_str = today.format("%Y-%m-%d").to_string();
        let floor_str = floor.format("%Y-%m-%d").to_string();
        let empty_legacy = BTreeSet::new();
        let inputs = RollingInputsRef {
            local_raw_events: &self.rolling_events,
            daily_metrics: &self.dashboard_metrics,
            session_metrics: &self.dashboard_sessions,
            legacy_local_days: &empty_legacy,
            today: &today_str,
            retention_floor: &floor_str,
        };
        let spent = selected_period_cost_ref(&state.reset_cycle, interval, &inputs, &[], tz)
            .unwrap_or(Decimal::ZERO);

        Self {
            generated_at: self.generated_at,
            active_boundary_at: interval.0,
            cost_since_reset_usd: spent,
            budget: BudgetSummary::new(spent, state.budget_usd),
            reset_cycle: state.reset_cycle.clone(),
            refresh_interval_seconds: state
                .refresh_interval_seconds
                .unwrap_or(self.refresh_interval_seconds),
            points: self.points.clone(),
            dashboard_metrics: self.dashboard_metrics.clone(),
            dashboard_sessions: self.dashboard_sessions.clone(),
            rolling_events: self.rolling_events.clone(),
        }
    }
}

pub fn local_day_bounds(day: NaiveDate, tz: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = local_midnight_utc(day, tz);
    let end = local_midnight_utc(day.succ_opt().unwrap_or(day), tz);
    (start, end)
}

fn discover_local_files(
    plan: &MachineSessionSourcePlan,
    antigravity_root: Option<&Path>,
) -> Vec<DiscoveredFile> {
    let mut out = Vec::new();
    for root in &plan.claude_roots {
        collect_jsonl(root, &mut |path| {
            out.push(DiscoveredFile {
                path: fs::canonicalize(&path).unwrap_or(path),
                context: ScanContext::Claude,
            });
        });
    }
    for root in &plan.codex_roots {
        collect_jsonl(root, &mut |path| {
            out.push(DiscoveredFile {
                path: fs::canonicalize(&path).unwrap_or(path),
                context: ScanContext::Codex(CodexScanContext::default()),
            });
        });
    }
    if let Some(agy_root) = antigravity_root {
        let workspaces = antigravity::workspaces(agy_root).unwrap_or_default();
        if let Ok(transcripts) = antigravity::transcripts(agy_root) {
            for (conv_id, path) in transcripts {
                let directory = workspaces.get(&conv_id).cloned();
                out.push(DiscoveredFile {
                    path: fs::canonicalize(&path).unwrap_or(path),
                    context: ScanContext::Antigravity {
                        conversation: conv_id,
                        directory,
                    },
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn collect_jsonl(root: &Path, push: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            collect_jsonl(&path, push);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            push(path);
        }
    }
}

fn scan_historical_range(
    scanner: &mut LocalUsageScanner,
    pricing: &PricingEngine,
    files: &[DiscoveredFile],
    since: NaiveDate,
    through: NaiveDate,
    tz: Tz,
) -> Vec<PricedUsageEvent> {
    let since_str = since.format("%Y-%m-%d").to_string();
    let mut by_id: BTreeMap<String, TimestampedUsageEvent> = BTreeMap::new();
    for item in files {
        if let Ok((events, _)) =
            scanner.scan_historical_once(&item.path, &since_str, tz, item.context.clone())
        {
            for ev in events {
                let d = ev.timestamp.with_timezone(&tz).date_naive();
                if d >= since && d <= through {
                    insert_dedup(&mut by_id, ev);
                }
            }
        }
    }
    let mut out: Vec<PricedUsageEvent> = by_id.values().map(|ev| pricing.price_event(ev)).collect();
    out.sort_by_key(|p| p.event.timestamp);
    out
}

fn insert_dedup(map: &mut BTreeMap<String, TimestampedUsageEvent>, ev: TimestampedUsageEvent) {
    if ev.agent == "claude"
        && map
            .get(&ev.identity)
            .is_some_and(|existing| existing.timestamp > ev.timestamp)
    {
        return;
    }
    map.insert(ev.identity.clone(), ev);
}
