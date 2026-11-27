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
    modified_unix_secs: i64,
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

    /// Close the open SQLite handle and reset in-memory scan/block caches so that deleting the
    /// on-disk cache file forces a clean full re-scan on the next refresh.
    pub fn reset_cache_state(&mut self) {
        self.cache = None;
        self.scanner = LocalUsageScanner::new();
        self.block_builder = BlockBuilder::new();
    }

    fn ensure_cache(&mut self) -> Result<&mut UsageAggregationCache, String> {
        if self.cache.is_none() {
            let opened = UsageAggregationCache::open(&self.cache_path, "local")
                .map_err(|e| format!("failed to open local aggregation cache: {e}"))?;
            self.cache = Some(opened);
        }
        Ok(self.cache.as_mut().expect("initialized"))
    }

    /// Fast (< 5ms) SQLite-only pre-seed snapshot for instant startup rendering of cached
    /// historical days while today's live scan runs asynchronously in the background.
    pub fn load_cached_snapshot(
        &mut self,
        descriptor: &MachineDescriptor,
        state: &AppState,
        config: &AppConfiguration,
        now: DateTime<Utc>,
        tz: Tz,
    ) -> Option<CostSnapshot> {
        let plan = MachineSessionSourcePlan::resolve_local(descriptor, &self.env);
        let fingerprint = plan.fingerprint().to_owned();
        let now_secs = now.timestamp_millis() as f64 / 1000.0;
        let retention_days = i64::from(config.cache_retention_days);
        let cache = self.ensure_cache().ok()?;
        let loaded = cache.load(now_secs, retention_days, Some(&fingerprint)).ok()??;
        let calc = ResetWindowCalculator::new(tz);
        let interval = calc
            .aggregation_interval(state.reset_cycle.clone(), now)
            .unwrap_or((now, now));
        let today = now.with_timezone(&tz).date_naive();
        let today_str = today.format("%Y-%m-%d").to_string();
        let empty_legacy = BTreeSet::new();
        let rolling_inputs = RollingInputsRef {
            local_raw_events: &[],
            daily_metrics: &loaded.metrics,
            session_metrics: &loaded.sessions,
            legacy_local_days: &empty_legacy,
            today: &today_str,
            retention_floor: &today_str,
        };
        let spent = selected_period_cost_ref(&state.reset_cycle, interval, &rolling_inputs, &[], tz)
            .unwrap_or(Decimal::ZERO);
        Some(CostSnapshot {
            generated_at: now,
            active_boundary_at: interval.0,
            cost_since_reset_usd: spent,
            budget: BudgetSummary::new(spent, state.budget_usd),
            reset_cycle: state.reset_cycle.clone(),
            refresh_interval_seconds: state
                .refresh_interval_seconds
                .unwrap_or(config.poll_interval_seconds),
            points: vec![],
            dashboard_metrics: loaded.metrics,
            dashboard_sessions: loaded.sessions,
            rolling_events: vec![],
        })
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
        self.refresh_local_with_progress(descriptor, state, config, now, tz, desired_from, &mut |_| {})
    }

    /// Refresh local usage newest-first using concurrent worker threads, invoking `on_progress`
    /// with intermediate snapshots as early waves complete.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh_local_with_progress(
        &mut self,
        descriptor: &MachineDescriptor,
        state: &AppState,
        config: &AppConfiguration,
        now: DateTime<Utc>,
        tz: Tz,
        desired_from: Option<NaiveDate>,
        on_progress: &mut dyn FnMut(CostSnapshot),
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

        let interval_start_day = interval.0.with_timezone(&tz).date_naive();
        let initial_from = interval_start_day.min(today);
        let points_from = desired_from.map_or(initial_from, |d| d.min(initial_from));

        // 1. Open SQLite cache FIRST and classify [backfill_start, yesterday] so we only scan
        // raw JSONL files for days that are uncovered or needed for the active rolling window.
        let now_secs = now.timestamp_millis() as f64 / 1000.0;
        let retention_days = i64::from(config.cache_retention_days);
        let cache = self.ensure_cache()?;
        let generation = cache
            .begin_source_attempt(&fingerprint)
            .map_err(|e| format!("cache begin_source_attempt failed: {e}"))?;
        let pre_load = cache
            .load(now_secs, retention_days, Some(&fingerprint))
            .map_err(|e| format!("cache load failed: {e}"))?;

        let mut legacy_local_days: BTreeSet<String> = BTreeSet::new();
        let mut uncovered_days: Vec<(NaiveDate, String)> = Vec::new();
        let yesterday = today.pred_opt().unwrap_or(today);
        let backfill_start = points_from.min(retention_floor);
        if backfill_start <= yesterday {
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
        }

        // Compute the narrowest live scan window: always includes `today`, any uncovered day in
        // `[retention_floor, yesterday]`, and `interval_start_day` for sub-day rolling cycles.
        let mut live_scan_floor = today;
        if matches!(
            state.reset_cycle,
            ResetCycle::Hourly | ResetCycle::CustomHours(_)
        ) {
            live_scan_floor = live_scan_floor.min(interval_start_day.max(retention_floor));
        }
        for (day, _) in &uncovered_days {
            if *day >= retention_floor {
                live_scan_floor = live_scan_floor.min(*day);
            }
        }
        let live_scan_floor_str = live_scan_floor.format("%Y-%m-%d").to_string();

        // Pin active boundary slot only for long CustomHours(n) windows starting before retention_floor.
        let boundary_day_opt = if matches!(state.reset_cycle, ResetCycle::CustomHours(_))
            && interval_start_day < retention_floor
        {
            Some(interval_start_day.format("%Y-%m-%d").to_string())
        } else {
            None
        };
        self.scanner
            .set_active_boundary(boundary_day_opt.as_deref(), &live_scan_floor_str);

        let old_days: Vec<NaiveDate> = uncovered_days
            .iter()
            .map(|(d, _)| *d)
            .filter(|d| *d < retention_floor)
            .collect();
        let earliest_needed_day = [
            Some(live_scan_floor),
            old_days.first().copied(),
            boundary_day_opt
                .as_deref()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
            (points_from < live_scan_floor).then_some(points_from),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(today);
        let (today_start, today_end) = local_day_bounds(today, tz);
        // When `live_scan_floor == today` and we have a persisted 15-minute watermark for today
        // (and no sub-day rolling cycle requiring earlier raw events), only scan files modified
        // at or after `live_watermark_ts` and reuse SQLite's finalized `[today_start, watermark)` buckets.
        let cached_today_watermark: Option<DateTime<Utc>> = if live_scan_floor == today
            && !matches!(
                state.reset_cycle,
                ResetCycle::Hourly | ResetCycle::CustomHours(_)
            )
            && old_days.is_empty()
            && boundary_day_opt.is_none()
            && points_from >= today
        {
            self.cache
                .as_ref()
                .and_then(|c| c.live_day_watermark(&today_str))
                .and_then(|ts| DateTime::from_timestamp(ts, 0))
                .filter(|wm| *wm > today_start && *wm <= now)
        } else {
            None
        };

        let discover_mtime_cutoff = if let Some(wm) = cached_today_watermark {
            wm.timestamp()
        } else if earliest_needed_day == today {
            today_start.timestamp()
        } else {
            local_midnight_utc(earliest_needed_day - Duration::days(1), tz).timestamp()
        };

        // Discover only local log files whose mtime could contain uncached events.
        let discovered = discover_local_files(
            &plan,
            self.antigravity_root.as_deref(),
            Some(discover_mtime_cutoff),
        );

        // 2. Scan recent [live_scan_floor, today] newest-first using time-period slices
        // dispatched across up to 5 concurrent workers, publishing each slice as it completes.
        let recent_mtime_cutoff = if let Some(wm) = cached_today_watermark {
            wm.timestamp()
        } else if live_scan_floor == today {
            today_start.timestamp()
        } else {
            local_midnight_utc(live_scan_floor - Duration::days(1), tz).timestamp()
        };
        let recent_files: Vec<DiscoveredFile> = discovered
            .iter()
            .filter(|item| item.modified_unix_secs >= recent_mtime_cutoff)
            .cloned()
            .collect();
        let mut recent_by_id: BTreeMap<String, TimestampedUsageEvent> = BTreeMap::new();
        let mut transitions: BTreeMap<PathBuf, ScanFileTransition> = BTreeMap::new();
        let mut contributors: BTreeMap<PathBuf, BlockContributor> = BTreeMap::new();

        let time_slices = partition_files_by_recency(&recent_files, now.timestamp());
        let total_slices = time_slices.len();
        let mut completed_slices = 0usize;

        run_time_slice_workers(
            &mut self.scanner,
            time_slices,
            &live_scan_floor_str,
            tz,
            5,
            &mut recent_by_id,
            &mut transitions,
            &mut contributors,
            &mut |recent_map| {
                completed_slices += 1;
                if completed_slices >= total_slices {
                    return;
                }
                let partial_priced: Vec<PricedUsageEvent> = recent_map
                    .values()
                    .map(|ev| self.pricing.price_event(ev))
                    .collect();
                let partial_today: Vec<PricedUsageEvent> = partial_priced
                    .iter()
                    .filter(|p| p.event.timestamp.with_timezone(&tz).date_naive() == today)
                    .cloned()
                    .collect();
                if let Ok((p_daily, p_sessions)) = aggregate_events(&partial_today, tz) {
                    let (today_start, _) = local_day_bounds(today, tz);
                    let mut partial_all_daily: Vec<MetricRecord> = pre_load
                        .as_ref()
                        .map(|p| {
                            p.metrics
                                .iter()
                                .filter(|m| m.date != today_str)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    partial_all_daily.extend(p_daily);
                    let mut partial_all_sessions: Vec<SessionMetricRecord> = pre_load
                        .as_ref()
                        .map(|p| {
                            p.sessions
                                .iter()
                                .filter(|s| s.timestamp < today_start)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    partial_all_sessions.extend(p_sessions);
                    let partial_inputs = RollingInputsRef {
                        local_raw_events: &partial_priced,
                        daily_metrics: &partial_all_daily,
                        session_metrics: &partial_all_sessions,
                        legacy_local_days: &legacy_local_days,
                        today: &today_str,
                        retention_floor: &live_scan_floor_str,
                    };
                    let partial_cost = selected_period_cost_ref(
                        &state.reset_cycle,
                        interval,
                        &partial_inputs,
                        &[],
                        tz,
                    )
                    .unwrap_or(Decimal::ZERO);
                    on_progress(CostSnapshot {
                        generated_at: now,
                        active_boundary_at: interval.0,
                        cost_since_reset_usd: partial_cost,
                        budget: BudgetSummary::new(partial_cost, state.budget_usd),
                        reset_cycle: state.reset_cycle.clone(),
                        refresh_interval_seconds: state
                            .refresh_interval_seconds
                            .unwrap_or(config.poll_interval_seconds),
                        points: vec![],
                        dashboard_metrics: partial_all_daily,
                        dashboard_sessions: partial_all_sessions,
                        rolling_events: partial_priced,
                    });
                }
            },
        );

        let recent_priced: Vec<PricedUsageEvent> = recent_by_id
            .values()
            .map(|ev| self.pricing.price_event(ev))
            .collect();

        // 3. If long CustomHours(n) pinned a boundary day < retention_floor, scan it via pinned slot.
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

        let mut recent_by_day: BTreeMap<NaiveDate, Vec<PricedUsageEvent>> = BTreeMap::new();
        for p in &recent_priced {
            let d = p.event.timestamp.with_timezone(&tz).date_naive();
            recent_by_day.entry(d).or_default().push(p.clone());
        }

        let mut wrote_backfill = false;
        if !uncovered_days.is_empty() {
            // Batch all uncovered days older than retention_floor into a single historical scan pass.
            let mut historical_by_day: BTreeMap<NaiveDate, Vec<PricedUsageEvent>> = BTreeMap::new();
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

        let today_events_all: &[PricedUsageEvent] =
            recent_by_day.get(&today).map_or(&[], Vec::as_slice);
        // When reusing cached `[today_start, wm)` 15-minute buckets from SQLite, only aggregate
        // newly scanned events in `[wm, today_end)` so the two partitions are strictly disjoint.
        let today_events_filtered: Vec<PricedUsageEvent> = if let Some(wm) = cached_today_watermark
        {
            today_events_all
                .iter()
                .filter(|p| p.event.timestamp >= wm)
                .cloned()
                .collect()
        } else {
            today_events_all.to_vec()
        };
        let (fresh_today_daily, fresh_today_sessions) =
            aggregate_events(&today_events_filtered, tz).map_err(|e| e.to_owned())?;

        // Merge cached `[today_start, wm)` sessions with fresh `[wm, today_end)` sessions.
        let session_replace_from = cached_today_watermark.unwrap_or(today_start);
        let mut today_merged_sessions: Vec<SessionMetricRecord> = loaded
            .as_ref()
            .map(|p| {
                p.sessions
                    .iter()
                    .filter(|s| s.timestamp >= today_start && s.timestamp < session_replace_from)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        today_merged_sessions.extend(fresh_today_sessions);

        // Recompute today's daily totals from the merged disjoint 15-minute sessions when using a watermark.
        let today_daily: Vec<MetricRecord> = if cached_today_watermark.is_some() {
            let mut by_key: BTreeMap<(String, String, Option<String>), MetricRecord> =
                BTreeMap::new();
            for s in &today_merged_sessions {
                let key = (s.agent.clone(), s.model.clone(), s.directory.clone());
                let entry = by_key.entry(key).or_insert_with(|| MetricRecord {
                    date: today_str.clone(),
                    agent: s.agent.clone(),
                    model: s.model.clone(),
                    cost_usd: Decimal::ZERO,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    total_tokens: 0,
                    machine: "local".into(),
                    directory: s.directory.clone(),
                    effort: None,
                });
                entry.cost_usd += s.cost_usd;
                entry.input_tokens = entry.input_tokens.saturating_add(s.input_tokens);
                entry.output_tokens = entry.output_tokens.saturating_add(s.output_tokens);
                entry.cache_creation_tokens = entry
                    .cache_creation_tokens
                    .saturating_add(s.cache_creation_tokens);
                entry.cache_read_tokens =
                    entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                entry.total_tokens = entry.total_tokens.saturating_add(s.total_tokens);
            }
            by_key.into_values().collect()
        } else {
            fresh_today_daily
        };

        // Persist today's 15-minute buckets and aligned 15-minute watermark to SQLite
        // (without marking today complete in coverage_ranges) so restarts only scan new files.
        let today_start_s = today_start.timestamp_millis() as f64 / 1000.0;
        let today_end_s = today_end.timestamp_millis() as f64 / 1000.0;
        let replace_from_s = session_replace_from.timestamp_millis() as f64 / 1000.0;
        let new_watermark_unix = (now.timestamp().div_euclid(900) * 900).max(today_start.timestamp());
        if !today_events_filtered.is_empty() || cached_today_watermark.is_none() {
            if let Ok(fresh_buckets) = events_to_session_buckets(
                &today_events_filtered,
                today_start_s,
                today_end_s,
                "local",
            ) {
                let _ = self.cache.as_mut().expect("opened").save_live_day(
                    &today_str,
                    today_start_s,
                    today_end_s,
                    replace_from_s,
                    new_watermark_unix,
                    &today_daily,
                    &fresh_buckets,
                    &fingerprint,
                    generation,
                );
            }
        }

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
        all_sessions.extend(today_merged_sessions);
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
            live_scan_floor,
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
            retention_floor: &live_scan_floor_str,
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
    mtime_cutoff_unix_secs: Option<i64>,
) -> Vec<DiscoveredFile> {
    let mut out = Vec::new();
    for root in &plan.claude_roots {
        collect_jsonl(root, mtime_cutoff_unix_secs, &mut |path, modified_unix_secs| {
            out.push(DiscoveredFile {
                path: fs::canonicalize(&path).unwrap_or(path),
                context: ScanContext::Claude,
                modified_unix_secs,
            });
        });
    }
    for root in &plan.codex_roots {
        collect_jsonl(root, mtime_cutoff_unix_secs, &mut |path, modified_unix_secs| {
            out.push(DiscoveredFile {
                path: fs::canonicalize(&path).unwrap_or(path),
                context: ScanContext::Codex(CodexScanContext::default()),
                modified_unix_secs,
            });
        });
    }
    if let Some(agy_root) = antigravity_root {
        let workspaces = antigravity::workspaces(agy_root).unwrap_or_default();
        if let Ok(transcripts) = antigravity::transcripts(agy_root) {
            for (conv_id, path) in transcripts {
                let mtime = file_mtime_unix_secs(&path).unwrap_or(i64::MAX);
                if let Some(cutoff) = mtime_cutoff_unix_secs {
                    if mtime < cutoff {
                        continue;
                    }
                }
                let directory = workspaces.get(&conv_id).cloned();
                out.push(DiscoveredFile {
                    path: fs::canonicalize(&path).unwrap_or(path),
                    context: ScanContext::Antigravity {
                        conversation: conv_id,
                        directory,
                    },
                    modified_unix_secs: mtime,
                });
            }
        }
    }
    // Sort newest-first so progressive waves render the most recent activity first.
    out.sort_by(|a, b| {
        b.modified_unix_secs
            .cmp(&a.modified_unix_secs)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn file_mtime_unix_secs(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_secs() as i64)
}

fn collect_jsonl(
    root: &Path,
    mtime_cutoff_unix_secs: Option<i64>,
    push: &mut impl FnMut(PathBuf, i64),
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            collect_jsonl(&path, mtime_cutoff_unix_secs, push);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = file_mtime_unix_secs(&path).unwrap_or(i64::MAX);
            if let Some(cutoff) = mtime_cutoff_unix_secs {
                if mtime < cutoff {
                    continue;
                }
            }
            push(path, mtime);
        }
    }
}

/// Partition newest-first files into time-period slices (last 1h, 1-3h, 3-6h, 6-12h, 12-24h,
/// and per-day older buckets) so concurrent workers finish the newest visible period first.
fn partition_files_by_recency(
    files: &[DiscoveredFile],
    now_unix_secs: i64,
) -> Vec<Vec<DiscoveredFile>> {
    let mut by_bucket: BTreeMap<i64, Vec<DiscoveredFile>> = BTreeMap::new();
    for item in files {
        let age = now_unix_secs.saturating_sub(item.modified_unix_secs).max(0);
        let bucket_key = if age < 3_600 {
            0 // last 1 hour
        } else if age < 3 * 3_600 {
            1 // 1..3 hours ago
        } else if age < 6 * 3_600 {
            2 // 3..6 hours ago
        } else if age < 12 * 3_600 {
            3 // 6..12 hours ago
        } else if age < 24 * 3_600 {
            4 // 12..24 hours ago
        } else {
            5 + (age / 86_400) // per-day slices going backward
        };
        by_bucket.entry(bucket_key).or_default().push(item.clone());
    }
    // Split any very large slice (> 120 files) into sub-slices so workers stay balanced
    // and publish progress incrementally without waiting on one huge slice.
    let mut slices = Vec::new();
    for (_, group) in by_bucket {
        for chunk in group.chunks(120) {
            slices.push(chunk.to_vec());
        }
    }
    slices
}

type WaveFileOutcome = (PathBuf, ScanFileTransition, Vec<TimestampedUsageEvent>);

struct SliceJob {
    files: Vec<DiscoveredFile>,
    scanner: LocalUsageScanner,
}

#[allow(clippy::too_many_arguments)]
fn run_time_slice_workers(
    scanner: &mut LocalUsageScanner,
    slices: Vec<Vec<DiscoveredFile>>,
    since: &str,
    tz: Tz,
    max_concurrency: usize,
    recent_by_id: &mut BTreeMap<String, TimestampedUsageEvent>,
    transitions: &mut BTreeMap<PathBuf, ScanFileTransition>,
    contributors: &mut BTreeMap<PathBuf, BlockContributor>,
    on_slice_complete: &mut dyn FnMut(&BTreeMap<String, TimestampedUsageEvent>),
) {
    if slices.is_empty() {
        return;
    }

    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(max_concurrency.max(1))
        .min(slices.len());

    // Pre-seed each slice job with any existing cached scanner entries for its files.
    let mut queue = std::collections::VecDeque::with_capacity(slices.len());
    for slice_files in slices {
        let mut local_scanner = LocalUsageScanner::new();
        for item in &slice_files {
            if let Some(existing) = scanner.entries.remove(&item.path) {
                local_scanner.entries.insert(item.path.clone(), existing);
            }
        }
        queue.push_back(SliceJob {
            files: slice_files,
            scanner: local_scanner,
        });
    }

    let shared_queue = std::sync::Mutex::new(queue);
    let (tx, rx) = std::sync::mpsc::channel::<(LocalUsageScanner, Vec<WaveFileOutcome>)>();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue_ref = &shared_queue;
            let tx_clone = tx.clone();
            scope.spawn(move || loop {
                let job = {
                    let Ok(mut guard) = queue_ref.lock() else {
                        break;
                    };
                    guard.pop_front()
                };
                let Some(mut job) = job else {
                    break;
                };
                let mut local_out = Vec::with_capacity(job.files.len());
                for item in &job.files {
                    if let Ok((events, transition)) =
                        job.scanner
                            .scan_file(&item.path, since, tz, item.context.clone())
                    {
                        local_out.push((item.path.clone(), transition, events));
                    }
                }
                if tx_clone.send((job.scanner, local_out)).is_err() {
                    break;
                }
            });
        }
        drop(tx);

        // Receive each completed time slice as soon as its worker finishes and publish live progress.
        for (local_scanner, local_out) in rx {
            scanner.stats.files_reused += local_scanner.stats.files_reused;
            scanner.stats.files_resumed += local_scanner.stats.files_resumed;
            scanner.stats.files_rescanned += local_scanner.stats.files_rescanned;
            scanner.stats.bytes_read += local_scanner.stats.bytes_read;
            for (path, entry) in local_scanner.entries {
                scanner.entries.insert(path, entry);
            }
            for (path, transition, events) in local_out {
                if let Some(entry) = scanner.entries.get(&path) {
                    contributors.insert(
                        path.clone(),
                        BlockContributor {
                            canonical_path: path.clone(),
                            file_size: entry.file_size,
                            modified: entry.modified,
                            scanned_through: entry.scanned_through,
                            boundary: entry.boundary.clone(),
                        },
                    );
                }
                transitions.insert(path, transition);
                for ev in events {
                    insert_dedup(recent_by_id, ev);
                }
            }
            on_slice_complete(recent_by_id);
        }
    });
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
    let mtime_cutoff = local_midnight_utc(since - Duration::days(1), tz).timestamp();
    let candidates: Vec<&DiscoveredFile> = files
        .iter()
        .filter(|item| item.modified_unix_secs >= mtime_cutoff)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(5)
        .min(candidates.len());

    let mut buckets: Vec<Vec<&DiscoveredFile>> = (0..worker_count).map(|_| Vec::new()).collect();
    for (idx, item) in candidates.into_iter().enumerate() {
        buckets[idx % worker_count].push(item);
    }

    let results: Vec<(ScanStats, Vec<TimestampedUsageEvent>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|items| {
                let since_ref = &since_str;
                scope.spawn(move || {
                    let mut local_scanner = LocalUsageScanner::new();
                    let mut local_events = Vec::new();
                    for item in items {
                        if let Ok((events, _)) = local_scanner.scan_historical_once(
                            &item.path,
                            since_ref,
                            tz,
                            item.context.clone(),
                        ) {
                            for ev in events {
                                let d = ev.timestamp.with_timezone(&tz).date_naive();
                                if d >= since && d <= through {
                                    local_events.push(ev);
                                }
                            }
                        }
                    }
                    (local_scanner.stats, local_events)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let mut by_id: BTreeMap<String, TimestampedUsageEvent> = BTreeMap::new();
    for (stats, events) in results {
        scanner.stats.files_reused += stats.files_reused;
        scanner.stats.files_resumed += stats.files_resumed;
        scanner.stats.files_rescanned += stats.files_rescanned;
        scanner.stats.bytes_read += stats.bytes_read;
        for ev in events {
            insert_dedup(&mut by_id, ev);
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
