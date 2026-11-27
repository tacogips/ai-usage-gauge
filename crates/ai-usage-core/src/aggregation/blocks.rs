use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;

use crate::types::{CostRecord, PricedUsageEvent, ScanFileTransition};

/// Replay a complete, deduplicated event history from a known regenerative reset.
/// The caller must supply enough preceding history to establish the block phase.
pub fn build_blocks(
    events: &[PricedUsageEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Vec<CostRecord> {
    let (completed, open) = replay_blocks_from_state(events, None);
    let mut all = completed;
    if let Some(open) = open {
        all.push(open);
    }
    filter_blocks_in_range(all, range_start, range_end)
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenBlockState {
    pub block_start: DateTime<Utc>,
    pub last_event_timestamp: DateTime<Utc>,
    pub cost_usd: Decimal,
    pub models: BTreeSet<String>,
}

impl OpenBlockState {
    pub fn into_cost_record(self) -> CostRecord {
        CostRecord {
            timestamp: self.block_start,
            cost_usd: self.cost_usd,
            models: self.models.into_iter().collect(),
            machine: "local".into(),
            directory: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockContributor {
    pub canonical_path: PathBuf,
    pub file_size: u64,
    pub modified: Option<SystemTime>,
    pub scanned_through: u64,
    pub boundary: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockAnchor {
    pub points_from: NaiveDate,
    pub retention_floor: NaiveDate,
    pub timezone_id: String,
    pub completed_blocks: Vec<(CostRecord, DateTime<Utc>)>,
    pub open_block: Option<OpenBlockState>,
    pub contributors: BTreeMap<PathBuf, BlockContributor>,
}

#[derive(Debug, Default)]
pub struct BlockBuilder {
    anchor: Option<BlockAnchor>,
}

impl BlockBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn anchor(&self) -> Option<&BlockAnchor> {
        self.anchor.as_ref()
    }

    pub fn invalidate(&mut self) {
        self.anchor = None;
    }

    /// Discover a regenerative 5-hour reset preceding `points_from` by scanning backwards in
    /// day chunks using `load_chunk(since_day, through_day)`.
    pub fn discover_regenerative_prefix<F, E>(
        points_from: NaiveDate,
        tz: Tz,
        earliest_limit: Option<NaiveDate>,
        mut load_chunk: F,
    ) -> Result<Vec<PricedUsageEvent>, E>
    where
        F: FnMut(NaiveDate, NaiveDate) -> Result<Vec<PricedUsageEvent>, E>,
    {
        let points_start = local_midnight_utc(points_from, tz);
        let mut by_id: BTreeMap<String, PricedUsageEvent> = BTreeMap::new();
        let mut chunk_end = points_from.pred_opt().unwrap_or(points_from);
        let mut chunk_days = 2i64;
        let max_lookback_days = 60i64;
        let hard_floor = earliest_limit.unwrap_or_else(|| {
            points_from
                .checked_sub_signed(Duration::days(max_lookback_days))
                .unwrap_or(points_from)
        });

        loop {
            if chunk_end < hard_floor {
                break;
            }
            let chunk_start = chunk_end
                .checked_sub_signed(Duration::days(chunk_days - 1))
                .map(|d| d.max(hard_floor))
                .unwrap_or(hard_floor);
            let chunk = load_chunk(chunk_start, chunk_end)?;
            for ev in chunk {
                by_id.insert(ev.event.identity.clone(), ev);
            }
            let mut sorted: Vec<&PricedUsageEvent> = by_id.values().collect();
            sorted.sort_by_key(|e| e.event.timestamp);

            // Condition 1: >= 5h gap between consecutive sorted events where e_prev < start_of_day(points_from).
            let mut found_gap = false;
            for pair in sorted.windows(2) {
                if pair[0].event.timestamp < points_start
                    && pair[1].event.timestamp - pair[0].event.timestamp >= Duration::hours(5)
                {
                    found_gap = true;
                    break;
                }
            }
            if found_gap {
                break;
            }

            // Condition 2: verified >= 5h quiet interval [t_earliest - 5h, t_earliest) inside scanned prefix.
            let chunk_start_utc = local_midnight_utc(chunk_start, tz);
            if let Some(first) = sorted.first() {
                if first.event.timestamp - chunk_start_utc >= Duration::hours(5) {
                    break;
                }
            } else if points_start - chunk_start_utc >= Duration::hours(5) {
                break;
            }

            if chunk_start <= hard_floor {
                break;
            }
            chunk_end = chunk_start.pred_opt().unwrap_or(hard_floor);
            chunk_days = (chunk_days * 2).min(14);
        }

        let mut out: Vec<PricedUsageEvent> = by_id.into_values().collect();
        out.sort_by_key(|e| e.event.timestamp);
        Ok(out)
    }

    /// Build or incrementally update blocks using the append-preserving `BlockAnchor` at `retention_floor`.
    /// Returns `(blocks, anchor_rebuilt)`.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_anchor<F, E>(
        &mut self,
        points_from: NaiveDate,
        retention_floor: NaiveDate,
        today: NaiveDate,
        tz: Tz,
        recent_events: &[PricedUsageEvent],
        current_contributors: &BTreeMap<PathBuf, BlockContributor>,
        transitions: &BTreeMap<PathBuf, ScanFileTransition>,
        mut load_pre_floor: F,
    ) -> Result<(Vec<CostRecord>, bool), E>
    where
        F: FnMut(NaiveDate, NaiveDate) -> Result<Vec<PricedUsageEvent>, E>,
    {
        let retention_start = local_midnight_utc(retention_floor, tz);
        let range_start = local_midnight_utc(points_from, tz);
        let range_end = local_midnight_utc(today.succ_opt().unwrap_or(today), tz);

        let can_reuse = self.can_preserve_anchor(
            points_from,
            retention_floor,
            tz,
            current_contributors,
            transitions,
        );

        if can_reuse {
            let anchor = self.anchor.as_mut().expect("checked");
            // Advance anchor forward in memory if retention_floor moved forward monotonically.
            if retention_floor > anchor.retention_floor {
                let advance_events: Vec<PricedUsageEvent> = recent_events
                    .iter()
                    .filter(|e| e.event.timestamp < retention_start)
                    .cloned()
                    .collect();
                let (newly_completed, next_open) =
                    replay_blocks_from_state(&advance_events, anchor.open_block.clone());
                anchor.completed_blocks.extend(newly_completed);
                anchor.open_block = next_open.map(|(rec, last)| OpenBlockState {
                    block_start: rec.timestamp,
                    last_event_timestamp: last,
                    cost_usd: rec.cost_usd,
                    models: rec.models.into_iter().collect(),
                });
                anchor.retention_floor = retention_floor;
            }
            // Update tracked contributor metadata for spanning files that resumed cleanly.
            for (path, contrib) in current_contributors {
                if let Some(tracked) = anchor.contributors.get_mut(path) {
                    *tracked = contrib.clone();
                }
            }
            let recent_only: Vec<PricedUsageEvent> = recent_events
                .iter()
                .filter(|e| e.event.timestamp >= retention_start)
                .cloned()
                .collect();
            let (mut tail_completed, tail_open) =
                replay_blocks_from_state(&recent_only, anchor.open_block.clone());
            let mut all = anchor.completed_blocks.clone();
            all.append(&mut tail_completed);
            if let Some(open) = tail_open {
                all.push(open);
            }
            return Ok((filter_blocks_in_range(all, range_start, range_end), false));
        }

        // Full rebuild from regenerative reset.
        let mut pre_floor_events = if points_from < retention_floor {
            let mut prefix =
                Self::discover_regenerative_prefix(points_from, tz, None, &mut load_pre_floor)?;
            let middle_end = retention_floor.pred_opt().unwrap_or(retention_floor);
            if points_from <= middle_end {
                prefix.extend(load_pre_floor(points_from, middle_end)?);
            }
            prefix
        } else {
            Self::discover_regenerative_prefix(retention_floor, tz, None, &mut load_pre_floor)?
        };

        // Deduplicate pre-floor events and keep strictly < retention_start.
        let mut dedup: BTreeMap<String, PricedUsageEvent> = BTreeMap::new();
        for ev in pre_floor_events.drain(..) {
            if ev.event.timestamp < retention_start {
                dedup.insert(ev.event.identity.clone(), ev);
            }
        }
        // Also include any recent_events that happen to be < retention_start.
        for ev in recent_events {
            if ev.event.timestamp < retention_start {
                dedup.insert(ev.event.identity.clone(), ev.clone());
            }
        }
        let pre_sorted: Vec<PricedUsageEvent> = dedup.into_values().collect();
        let (completed_pre, open_pre) = replay_blocks_from_state(&pre_sorted, None);

        let open_state = open_pre.map(|(rec, last)| OpenBlockState {
            block_start: rec.timestamp,
            last_event_timestamp: last,
            cost_usd: rec.cost_usd,
            models: rec.models.into_iter().collect(),
        });

        let anchor = BlockAnchor {
            points_from,
            retention_floor,
            timezone_id: tz.name().to_owned(),
            completed_blocks: completed_pre.clone(),
            open_block: open_state.clone(),
            contributors: current_contributors.clone(),
        };
        self.anchor = Some(anchor);

        let recent_only: Vec<PricedUsageEvent> = recent_events
            .iter()
            .filter(|e| e.event.timestamp >= retention_start)
            .cloned()
            .collect();
        let (mut tail_completed, tail_open) = replay_blocks_from_state(&recent_only, open_state);
        let mut all = completed_pre;
        all.append(&mut tail_completed);
        if let Some(open) = tail_open {
            all.push(open);
        }
        Ok((filter_blocks_in_range(all, range_start, range_end), true))
    }

    fn can_preserve_anchor(
        &self,
        points_from: NaiveDate,
        retention_floor: NaiveDate,
        tz: Tz,
        current_contributors: &BTreeMap<PathBuf, BlockContributor>,
        transitions: &BTreeMap<PathBuf, ScanFileTransition>,
    ) -> bool {
        let Some(anchor) = &self.anchor else {
            return false;
        };
        if anchor.timezone_id != tz.name()
            || points_from < anchor.points_from
            || retention_floor < anchor.retention_floor
        {
            return false;
        }
        let floor_str = retention_floor.format("%Y-%m-%d").to_string();
        // Every tracked contributor must still exist and must have either Reused or Resumed with
        // only >= retention_floor appended events.
        for path in anchor.contributors.keys() {
            if !current_contributors.contains_key(path) {
                return false;
            }
            match transitions.get(path) {
                Some(ScanFileTransition::Reuse) | None => {}
                Some(ScanFileTransition::Resume { new_events, .. }) => {
                    for ev in new_events {
                        let ev_day = ev
                            .timestamp
                            .with_timezone(&tz)
                            .format("%Y-%m-%d")
                            .to_string();
                        if ev_day < floor_str {
                            return false;
                        }
                    }
                }
                Some(ScanFileTransition::Rescan) => return false,
            }
        }
        // Any newly discovered file that wasn't tracked must NOT have pre-floor events (if it was
        // rescanned, we only allow it if it's purely recent; otherwise rebuild).
        for (path, transition) in transitions {
            if !anchor.contributors.contains_key(path) {
                if let ScanFileTransition::Resume { new_events, .. } = transition {
                    if new_events.iter().any(|e| {
                        e.timestamp
                            .with_timezone(&tz)
                            .format("%Y-%m-%d")
                            .to_string()
                            < floor_str
                    }) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

pub fn local_midnight_utc(day: NaiveDate, tz: Tz) -> DateTime<Utc> {
    let naive = day.and_hms_opt(0, 0, 0).expect("valid midnight");
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(a, b) => a.min(b).with_timezone(&Utc),
        chrono::LocalResult::None => {
            for sec in 1..86_400 {
                if let Some(cand) = naive.checked_add_signed(Duration::seconds(sec)) {
                    match tz.from_local_datetime(&cand) {
                        chrono::LocalResult::Single(dt) => return dt.with_timezone(&Utc),
                        chrono::LocalResult::Ambiguous(a, b) => {
                            return a.min(b).with_timezone(&Utc)
                        }
                        chrono::LocalResult::None => {}
                    }
                }
            }
            DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        }
    }
}

type CompletedBlock = (CostRecord, DateTime<Utc>);

fn replay_blocks_from_state(
    events: &[PricedUsageEvent],
    initial_open: Option<OpenBlockState>,
) -> (Vec<CompletedBlock>, Option<CompletedBlock>) {
    let mut sorted: Vec<_> = events.iter().collect();
    sorted.sort_by_key(|event| event.event.timestamp);
    let mut blocks: Vec<(CostRecord, DateTime<Utc>)> = Vec::new();
    if let Some(open) = initial_open {
        let last = open.last_event_timestamp;
        blocks.push((open.into_cost_record(), last));
    }
    for priced in sorted {
        let timestamp = priced.event.timestamp;
        let new_block = blocks.last().is_none_or(|(block, last)| {
            timestamp >= block.timestamp + Duration::hours(5)
                || timestamp - *last >= Duration::hours(5)
        });
        if new_block {
            let seconds = timestamp.timestamp().div_euclid(3600) * 3600;
            let start = DateTime::from_timestamp(seconds, 0).expect("valid event hour");
            blocks.push((
                CostRecord {
                    timestamp: start,
                    cost_usd: priced.cost_usd,
                    models: vec![priced.event.model.clone()],
                    machine: "local".into(),
                    directory: None,
                },
                timestamp,
            ));
        } else if let Some((block, last)) = blocks.last_mut() {
            block.cost_usd += priced.cost_usd;
            if !block.models.contains(&priced.event.model) {
                block.models.push(priced.event.model.clone());
            }
            *last = timestamp;
        }
    }
    for (block, _) in &mut blocks {
        block.models.sort();
    }
    let open = blocks.pop();
    (blocks, open)
}

fn filter_blocks_in_range(
    blocks: Vec<(CostRecord, DateTime<Utc>)>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Vec<CostRecord> {
    blocks
        .into_iter()
        .filter_map(|(mut block, last)| {
            block.models.sort();
            (last >= range_start && block.timestamp < range_end).then_some(block)
        })
        .collect()
}
