use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::registry::{MachineRegistryStore, MachineValidation};
use crate::types::{LocalSessionSources, MachineDescriptor, MachineRegistry};

pub use crate::types::{DirectoryNameLifecycle, MachineRuntimeReconciler};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationError {
    Conflict,
    NotFound,
    Invalid(String),
    Persistence,
    ReconciliationFailed { reconciliation_required: bool },
    ReconciliationRequired,
}

struct State {
    registry: MachineRegistry,
    revision: u64,
    reconciliation_required: bool,
    cleanup_pending: BTreeSet<String>,
}

pub struct MachineRegistryMutationOwner<R, D> {
    store: MachineRegistryStore,
    runtime: Arc<R>,
    metadata: Arc<D>,
    state: Mutex<State>,
}

impl<R: MachineRuntimeReconciler, D: DirectoryNameLifecycle> MachineRegistryMutationOwner<R, D> {
    pub fn new(
        store: MachineRegistryStore,
        registry: MachineRegistry,
        runtime: Arc<R>,
        metadata: Arc<D>,
    ) -> Self {
        Self {
            store,
            runtime,
            metadata,
            state: Mutex::new(State {
                registry,
                revision: 0,
                reconciliation_required: false,
                cleanup_pending: BTreeSet::new(),
            }),
        }
    }

    pub async fn current(&self) -> MachineRegistry {
        self.state.lock().await.registry.clone()
    }
    pub async fn metadata_cleanup_pending_machine_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .cleanup_pending
            .iter()
            .cloned()
            .collect()
    }
    pub async fn retry_pending_metadata_cleanup(&self) -> Vec<String> {
        let mut state = self.state.lock().await;
        let ids: Vec<_> = state.cleanup_pending.iter().cloned().collect();
        for id in ids {
            if state.registry.machines.iter().any(|m| m.id == id)
                || self.metadata.purge_deleted(&id).is_ok()
            {
                state.cleanup_pending.remove(&id);
            }
        }
        state.cleanup_pending.iter().cloned().collect()
    }
    pub async fn create(
        &self,
        descriptor: MachineDescriptor,
    ) -> Result<MachineRegistry, MutationError> {
        let mut state = self.state.lock().await;
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        if descriptor.id == "local"
            || descriptor.id == "all"
            || state
                .registry
                .machines
                .iter()
                .any(|m| m.id == descriptor.id)
        {
            return Err(MutationError::Conflict);
        }
        MachineValidation::validate(&descriptor)
            .map_err(|e| MutationError::Invalid(e.to_string()))?;
        let mut candidate = state.registry.clone();
        candidate.machines.push(descriptor);
        self.commit(&mut state, candidate).await
    }
    pub async fn replace(
        &self,
        id: &str,
        descriptor: MachineDescriptor,
    ) -> Result<MachineRegistry, MutationError> {
        let mut state = self.state.lock().await;
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        if id == "local" || id == "all" || id != descriptor.id {
            return Err(MutationError::Conflict);
        }
        MachineValidation::validate(&descriptor)
            .map_err(|e| MutationError::Invalid(e.to_string()))?;
        let mut candidate = state.registry.clone();
        let item = candidate
            .machines
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or(MutationError::NotFound)?;
        *item = descriptor;
        self.commit(&mut state, candidate).await
    }
    pub async fn patch(
        &self,
        id: &str,
        patch: impl FnOnce(&mut MachineDescriptor),
    ) -> Result<MachineRegistry, MutationError> {
        let mut state = self.state.lock().await;
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        if id == "local" || id == "all" {
            return Err(MutationError::Conflict);
        }
        let mut candidate = state.registry.clone();
        let item = candidate
            .machines
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or(MutationError::NotFound)?;
        patch(item);
        if item.id != id {
            return Err(MutationError::Conflict);
        }
        MachineValidation::validate(item).map_err(|e| MutationError::Invalid(e.to_string()))?;
        self.commit(&mut state, candidate).await
    }
    pub async fn patch_local(
        &self,
        sources: LocalSessionSources,
    ) -> Result<MachineRegistry, MutationError> {
        self.patch_local_with(|current| *current = sources).await
    }
    pub async fn patch_local_with(
        &self,
        patch: impl FnOnce(&mut LocalSessionSources),
    ) -> Result<MachineRegistry, MutationError> {
        let mut state = self.state.lock().await;
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        let mut candidate = state.registry.clone();
        patch(&mut candidate.local_session_sources);
        for path in candidate
            .local_session_sources
            .codex_session_dirs
            .iter()
            .chain(&candidate.local_session_sources.claude_config_dirs)
        {
            if !MachineValidation::is_valid_session_source_path(path) {
                return Err(MutationError::Invalid("invalid local source path".into()));
            }
        }
        self.commit(&mut state, candidate).await
    }
    pub async fn delete(&self, id: &str) -> Result<MachineRegistry, MutationError> {
        let mut state = self.state.lock().await;
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        if id == "local" || id == "all" {
            return Err(MutationError::Conflict);
        }
        let mut candidate = state.registry.clone();
        let before = candidate.machines.len();
        candidate.machines.retain(|m| m.id != id);
        if before == candidate.machines.len() {
            return Err(MutationError::NotFound);
        }
        self.commit(&mut state, candidate).await
    }
    async fn commit(
        &self,
        state: &mut State,
        candidate: MachineRegistry,
    ) -> Result<MachineRegistry, MutationError> {
        if state.reconciliation_required {
            return Err(MutationError::ReconciliationRequired);
        }
        let snapshot = self
            .store
            .snapshot()
            .map_err(|_| MutationError::Persistence)?;
        let previous = state.registry.clone();
        let old: BTreeSet<_> = previous.machines.iter().map(|m| m.id.as_str()).collect();
        let new: BTreeSet<_> = candidate.machines.iter().map(|m| m.id.as_str()).collect();
        let created: Vec<_> = new.difference(&old).copied().collect();
        let deleted: Vec<_> = old.difference(&new).copied().collect();
        for id in created {
            self.metadata
                .purge_for_reuse(id)
                .map_err(|_| MutationError::Persistence)?;
        }
        let mut prepared = false;
        for id in &deleted {
            if self.metadata.prepare_delete(id).is_err() {
                let failed = prepared
                    && self
                        .metadata
                        .reconcile_machine_registry(&active_machine_ids(&previous))
                        .is_err();
                state.reconciliation_required = failed;
                return Err(MutationError::ReconciliationFailed {
                    reconciliation_required: failed,
                });
            }
            prepared = true;
        }
        if self.store.save(&candidate).is_err() {
            let failed = prepared
                && self
                    .metadata
                    .reconcile_machine_registry(&active_machine_ids(&previous))
                    .is_err();
            state.reconciliation_required = failed;
            return Err(if failed {
                MutationError::ReconciliationFailed {
                    reconciliation_required: true,
                }
            } else {
                MutationError::Persistence
            });
        }
        let mut staged = true;
        for id in &deleted {
            if self.metadata.commit_delete(id).is_err() {
                staged = false;
            }
        }
        let runtime_failed = if staged {
            state.revision += 1;
            self.runtime
                .reconcile_registry(&candidate, state.revision)
                .await
                .is_err()
        } else {
            true
        };
        if runtime_failed {
            let mut failed = false;
            for id in &deleted {
                if self.metadata.rollback_delete(id).is_err() {
                    failed = true;
                }
            }
            if !failed && self.store.restore(snapshot.as_deref()).is_ok() {
                if !deleted.is_empty()
                    && self
                        .metadata
                        .reconcile_machine_registry(&active_machine_ids(&previous))
                        .is_err()
                {
                    failed = true;
                }
            } else {
                failed = true;
            }
            state.revision += 1;
            if self
                .runtime
                .reconcile_registry(&previous, state.revision)
                .await
                .is_err()
            {
                failed = true;
            }
            state.reconciliation_required = failed;
            return Err(MutationError::ReconciliationFailed {
                reconciliation_required: failed,
            });
        }
        for id in deleted {
            if self.metadata.purge_deleted(id).is_err() {
                state.cleanup_pending.insert(id.to_owned());
            }
        }
        state.registry = candidate.clone();
        Ok(candidate)
    }
}

fn active_machine_ids(registry: &MachineRegistry) -> Vec<String> {
    std::iter::once("local".to_owned())
        .chain(registry.machines.iter().map(|m| m.id.clone()))
        .collect()
}
