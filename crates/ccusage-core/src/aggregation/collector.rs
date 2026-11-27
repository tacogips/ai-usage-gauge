use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use tokio::sync::{Mutex, Notify, RwLock};

use crate::aggregation::merge::{CostSnapshotMerge, MachineSnapshotInput, MergedDashboardSnapshot};
use crate::types::{
    AppState, CostSnapshot, DashboardDataDisposition, DashboardRangeLoadProgress,
    MachineCollectionState, MachineDescriptor, MachineRegistry, MachineRuntimeReconciler,
    MachineStatus, SanitizedCollectionError,
};

pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CostSnapshot, SanitizedCollectionError>> + Send + 'a>>;

pub trait MachineSnapshotFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        descriptor: &'a MachineDescriptor,
        desired_from: Option<NaiveDate>,
        now: DateTime<Utc>,
    ) -> FetchFuture<'a>;
}

#[derive(Debug, Clone)]
struct StoredMachine {
    descriptor: MachineDescriptor,
    generation: u64,
    snapshot: Option<CostSnapshot>,
    status: MachineStatus,
}

#[derive(Debug, Default)]
struct StoreInner {
    revision: u64,
    machines: BTreeMap<String, StoredMachine>,
    range_progress: Option<DashboardRangeLoadProgress>,
}

#[derive(Debug, Clone, Default)]
pub struct MachineSnapshotStore {
    inner: Arc<RwLock<StoreInner>>,
}

impl MachineSnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn reconcile_registry(
        &self,
        registry: &MachineRegistry,
        revision: u64,
        refresh_interval_seconds: u32,
    ) {
        let mut guard = self.inner.write().await;
        if revision < guard.revision {
            return;
        }
        guard.revision = revision;

        let desired_ids: BTreeMap<String, &MachineDescriptor> = registry
            .machines
            .iter()
            .map(|m| (m.id.clone(), m))
            .collect();

        guard.machines.retain(|id, _| desired_ids.contains_key(id));

        for (id, desc) in desired_ids {
            if let Some(existing) = guard.machines.get_mut(&id) {
                let changed = existing.descriptor != *desc;
                existing.descriptor = desc.clone();
                existing.status.display_name = desc.display_name.clone();
                existing.status.enabled = desc.enabled;
                existing.status.refresh_interval_seconds = refresh_interval_seconds;
                if !desc.enabled {
                    existing.status.collection_state = MachineCollectionState::Disabled;
                } else if existing.status.collection_state == MachineCollectionState::Disabled {
                    existing.status.collection_state = if existing.snapshot.is_some() {
                        MachineCollectionState::Stale
                    } else {
                        MachineCollectionState::NeverCollected
                    };
                }
                if changed {
                    existing.generation = existing.generation.wrapping_add(1);
                }
            } else {
                let state = if desc.enabled {
                    MachineCollectionState::NeverCollected
                } else {
                    MachineCollectionState::Disabled
                };
                guard.machines.insert(
                    id.clone(),
                    StoredMachine {
                        descriptor: desc.clone(),
                        generation: 1,
                        snapshot: None,
                        status: MachineStatus {
                            id,
                            display_name: desc.display_name.clone(),
                            kind: desc.kind,
                            enabled: desc.enabled,
                            collection_state: state,
                            snapshot_available: false,
                            collection_in_progress: false,
                            stale: false,
                            coverage_start: None,
                            snapshot_generated_at: None,
                            last_attempt_at: None,
                            last_success_at: None,
                            consecutive_failure_count: 0,
                            unavailable_since: None,
                            stale_since: None,
                            last_error_at: None,
                            last_error: None,
                            last_hour_data_gap: None,
                            refresh_interval_seconds,
                        },
                    },
                );
            }
        }
    }

    pub async fn begin_attempt(
        &self,
        machine_id: &str,
        now: DateTime<Utc>,
    ) -> Option<(MachineDescriptor, u64)> {
        let mut guard = self.inner.write().await;
        let entry = guard.machines.get_mut(machine_id)?;
        if !entry.descriptor.enabled {
            return None;
        }
        entry.status.collection_in_progress = true;
        entry.status.last_attempt_at = Some(now);
        Some((entry.descriptor.clone(), entry.generation))
    }

    /// Commit a successful snapshot only if `generation` still matches (stale-generation fencing).
    pub async fn commit_success(
        &self,
        machine_id: &str,
        generation: u64,
        snapshot: CostSnapshot,
        now: DateTime<Utc>,
    ) -> bool {
        let mut guard = self.inner.write().await;
        let Some(entry) = guard.machines.get_mut(machine_id) else {
            return false;
        };
        if entry.generation != generation {
            return false;
        }
        entry.status.collection_in_progress = false;
        entry.status.collection_state = MachineCollectionState::Healthy;
        entry.status.snapshot_available = true;
        entry.status.stale = false;
        entry.status.snapshot_generated_at = Some(snapshot.generated_at);
        entry.status.last_success_at = Some(now);
        entry.status.consecutive_failure_count = 0;
        entry.status.unavailable_since = None;
        entry.status.stale_since = None;
        entry.status.last_error = None;
        entry.status.coverage_start = snapshot.dashboard_metrics.first().map(|m| m.date.clone());
        entry.snapshot = Some(snapshot);
        true
    }

    pub async fn commit_failure(
        &self,
        machine_id: &str,
        generation: u64,
        error: SanitizedCollectionError,
        now: DateTime<Utc>,
    ) -> bool {
        let mut guard = self.inner.write().await;
        let Some(entry) = guard.machines.get_mut(machine_id) else {
            return false;
        };
        if entry.generation != generation {
            return false;
        }
        entry.status.collection_in_progress = false;
        entry.status.consecutive_failure_count =
            entry.status.consecutive_failure_count.saturating_add(1);
        entry.status.last_error_at = Some(now);
        entry.status.last_error = Some(error);
        if entry.status.unavailable_since.is_none() {
            entry.status.unavailable_since = Some(now);
        }
        if entry.snapshot.is_some() {
            entry.status.stale = true;
            if entry.status.stale_since.is_none() {
                entry.status.stale_since = Some(now);
            }
            entry.status.collection_state = MachineCollectionState::Stale;
        } else {
            entry.status.collection_state = MachineCollectionState::Error;
        }
        true
    }

    pub async fn mark_stale_if_expired(&self, now: DateTime<Utc>, stale_after: Duration) {
        let mut guard = self.inner.write().await;
        for entry in guard.machines.values_mut() {
            if !entry.descriptor.enabled {
                continue;
            }
            if let Some(last_ok) = entry.status.last_success_at {
                if now - last_ok > stale_after {
                    entry.status.stale = true;
                    if entry.status.stale_since.is_none() {
                        entry.status.stale_since = Some(last_ok + stale_after);
                    }
                    if entry.status.unavailable_since.is_none() {
                        entry.status.unavailable_since = entry.status.stale_since;
                    }
                    entry.status.collection_state = MachineCollectionState::Stale;
                }
            }
        }
    }

    pub async fn statuses(&self) -> Vec<MachineStatus> {
        let guard = self.inner.read().await;
        guard.machines.values().map(|e| e.status.clone()).collect()
    }

    pub async fn inputs(&self) -> Vec<MachineSnapshotInput> {
        let guard = self.inner.read().await;
        guard
            .machines
            .values()
            .map(|e| MachineSnapshotInput {
                status: e.status.clone(),
                snapshot: e.snapshot.clone(),
            })
            .collect()
    }

    pub async fn enabled_machine_ids(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        guard
            .machines
            .values()
            .filter(|e| e.descriptor.enabled)
            .map(|e| e.descriptor.id.clone())
            .collect()
    }

    pub async fn set_range_progress(&self, progress: Option<DashboardRangeLoadProgress>) {
        self.inner.write().await.range_progress = progress;
    }

    pub async fn range_progress(&self) -> Option<DashboardRangeLoadProgress> {
        self.inner.read().await.range_progress.clone()
    }
}

pub struct MachineCollector {
    pub store: MachineSnapshotStore,
    fetcher: Arc<dyn MachineSnapshotFetcher>,
    in_flight: Mutex<HashMap<String, Arc<Notify>>>,
    refresh_interval_seconds: u32,
}

impl MachineCollector {
    pub fn new(
        store: MachineSnapshotStore,
        fetcher: Arc<dyn MachineSnapshotFetcher>,
        refresh_interval_seconds: u32,
    ) -> Self {
        Self {
            store,
            fetcher,
            in_flight: Mutex::new(HashMap::new()),
            refresh_interval_seconds,
        }
    }

    /// Single-flight per-machine refresh: concurrent callers for the same machine wait for the
    /// active in-flight poll rather than spawning duplicate polls.
    pub async fn refresh_machine(
        &self,
        machine_id: &str,
        desired_from: Option<NaiveDate>,
        now: DateTime<Utc>,
    ) -> bool {
        let (notify, is_leader) = {
            let mut map = self.in_flight.lock().await;
            if let Some(existing) = map.get(machine_id) {
                (existing.clone(), false)
            } else {
                let n = Arc::new(Notify::new());
                map.insert(machine_id.to_owned(), n.clone());
                (n, true)
            }
        };

        if !is_leader {
            notify.notified().await;
            return true;
        }

        let result =
            if let Some((descriptor, gen)) = self.store.begin_attempt(machine_id, now).await {
                match self.fetcher.fetch(&descriptor, desired_from, now).await {
                    Ok(snap) => self.store.commit_success(machine_id, gen, snap, now).await,
                    Err(err) => {
                        self.store.commit_failure(machine_id, gen, err, now).await;
                        false
                    }
                }
            } else {
                false
            };

        {
            let mut map = self.in_flight.lock().await;
            map.remove(machine_id);
        }
        notify.notify_waiters();
        result
    }

    pub async fn refresh_all(&self, desired_from: Option<NaiveDate>, now: DateTime<Utc>) {
        let ids = self.store.enabled_machine_ids().await;
        let handles: Vec<_> = ids
            .iter()
            .map(|id| self.refresh_machine(id, desired_from, now))
            .collect();
        futures_join_all(handles).await;
    }

    pub async fn load_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        tz: Tz,
        now: DateTime<Utc>,
    ) -> DashboardRangeLoadProgress {
        let ids = self.store.enabled_machine_ids().await;
        let total = ids.len();
        let desired_from = Some(start.with_timezone(&tz).date_naive());
        let mut progress = DashboardRangeLoadProgress {
            requested_start: Some(start),
            requested_end: Some(end),
            completed: 0,
            total,
            is_loading: total > 0,
            is_partial: false,
            failed_machine_ids: Vec::new(),
        };
        self.store.set_range_progress(Some(progress.clone())).await;

        for id in ids {
            let ok = self.refresh_machine(&id, desired_from, now).await;
            progress.completed += 1;
            if !ok {
                progress.failed_machine_ids.push(id);
                progress.is_partial = true;
            }
            self.store.set_range_progress(Some(progress.clone())).await;
        }
        progress.is_loading = false;
        self.store.set_range_progress(Some(progress.clone())).await;
        progress
    }

    pub async fn merged_snapshot(
        &self,
        state: &AppState,
        requested: &str,
        disposition: DashboardDataDisposition,
        now: DateTime<Utc>,
        tz: Tz,
    ) -> MergedDashboardSnapshot {
        let inputs = self.store.inputs().await;
        CostSnapshotMerge::merge(&inputs, state, requested, disposition, now, tz)
    }
}

impl MachineRuntimeReconciler for MachineCollector {
    fn reconcile_registry<'a>(
        &'a self,
        registry: &'a MachineRegistry,
        revision: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.store
                .reconcile_registry(registry, revision, self.refresh_interval_seconds)
                .await;
            Ok(())
        })
    }
}

pub struct PollingService {
    collector: Arc<MachineCollector>,
    interval: StdDuration,
}

impl PollingService {
    pub fn new(collector: Arc<MachineCollector>, interval_seconds: u32) -> Self {
        Self {
            collector,
            interval: StdDuration::from_secs(u64::from(interval_seconds.max(1))),
        }
    }

    pub fn spawn(
        self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.collector.refresh_all(None, Utc::now()).await;
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        })
    }
}

async fn futures_join_all<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut out = Vec::with_capacity(futs.len());
    for f in futs {
        out.push(f.await);
    }
    out
}
